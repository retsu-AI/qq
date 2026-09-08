//! Application configuration to model-runtime composition.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
};

use qq_auth::{AuthError, CredentialStore, Secret, resolve_provider_credential};
use qq_config::{
    AwsAuth, BedrockAuth, ConfigError, ConfigLoader, ConfigSnapshot, EndpointMode, HttpAccess,
    HttpCredential, LoadRequest, PromotionOutcome, ProviderAccess, ProviderApi, ProviderAuth,
    ProviderConfig, WorkspaceGrant,
};
use qq_core::{
    ApprovalReviewer, GrantPromotionFuture, GrantSeedFuture, LoadedRuntime, PublishedEventStream,
    ReviewDecision, ReviewFuture, ReviewRequest, ReviewVerdict, RuntimeConfigError,
    RuntimeLoadError, RuntimeLoadFuture, RuntimeLoadRequest, RuntimeLoader, SessionRuntime,
    SessionRuntimeError, SessionRuntimeOptions, SpawnModelValidationFuture,
    WorkerRuntimeLoadFuture, WorkspaceGrantAuthority, WorkspaceGrantSeed,
    plan::{
        AgentProfile, CompiledAgentPlan, CredentialReference, HostSnapshot, PackSelection,
        PlanCompileError, ProviderDescriptor, SourceFingerprint,
    },
};
use qq_protocol::{
    AgentProfileId, AgentProfileSummary, ApprovalGrant, ApprovalMode, CapabilitySupport,
    CommandRequest, GenerationCapabilities, ModelCatalogRequest, ModelDescriptor, ModelSelection,
    PromptCacheCapabilities, ProviderRequestShapeIdentity, ProviderRequestShapeVersion,
    ResolvedModel, ResolvedModelVersion, RunFailureKind, SnapshotRequest, SubscribeRequest,
    WorkspaceGrantOutcome, WorkspaceId,
};
use qq_provider::{
    BedrockAuth as ProviderBedrockAuth, EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe,
    ProviderCompiler, ProviderError, ProviderRecipe, SecretRef,
};
use qq_server::{
    CommandFuture, DelegationFuture, ModelsFuture, ProfilesFuture, ServerHandler,
    ServerHandlerError, SnapshotFuture, WorkspaceToolsFuture,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    catalog::{DiscoveredModel, ModelDiscovery},
    plan::{
        CompiledGeneration, LiveBindings, PlanCache, PlanCacheError, PlanCacheLimits, PlanKey,
        PlanLookup,
    },
};

const MAX_MODEL_OPTIONS: usize = 4_096;
const MAX_DISCOVERY_PROVIDERS: usize = 4;

#[derive(Clone)]
pub struct RuntimeFactory {
    inner: Arc<RuntimeFactoryInner>,
}

struct RuntimeFactoryInner {
    config: ConfigLoader,
    credentials: CredentialStore,
    providers: ProviderCompiler,
    discovery: ModelDiscovery,
    mcp: crate::mcp::McpRegistryCache,
    plans: PlanCache,
}

impl RuntimeFactory {
    pub fn system() -> Result<Self, RuntimeBuildError> {
        Self::new(ConfigLoader::system()?, CredentialStore::system()?)
    }

    pub fn new(
        config: ConfigLoader,
        credentials: CredentialStore,
    ) -> Result<Self, RuntimeBuildError> {
        Ok(Self {
            inner: Arc::new(RuntimeFactoryInner {
                config,
                credentials,
                providers: ProviderCompiler::new()?,
                discovery: ModelDiscovery::new()?,
                mcp: crate::mcp::McpRegistryCache::new(),
                plans: PlanCache::new(PlanCacheLimits::default()),
            }),
        })
    }

    pub fn load(&self, request: &LoadRequest) -> Result<ConfigSnapshot, RuntimeBuildError> {
        self.inner.config.load(request).map_err(Into::into)
    }

    pub fn configured_model_options(&self, snapshot: &ConfigSnapshot) -> Vec<ModelDescriptor> {
        self.model_options_with_discovery(snapshot, &BTreeMap::new())
    }

    fn model_options_with_discovery(
        &self,
        snapshot: &ConfigSnapshot,
        discovered: &BTreeMap<String, Vec<DiscoveredModel>>,
    ) -> Vec<ModelDescriptor> {
        let allowed = snapshot.policy().allowed_providers();
        let denied = snapshot.policy().denied_providers();
        let mut options = Vec::new();
        'providers: for (provider_id, provider) in snapshot.providers() {
            if allowed.is_some_and(|allowed| !allowed.iter().any(|id| id == provider_id))
                || denied.iter().any(|id| id == provider_id)
                || !self.provider_authenticated(provider_id, provider)
            {
                continue;
            }
            for (model_id, metadata) in provider.models() {
                if options.len() >= MAX_MODEL_OPTIONS {
                    break 'providers;
                }
                options.push(ModelDescriptor {
                    provider: provider_id.clone(),
                    model: model_id.clone(),
                    name: metadata.name().map(str::to_owned),
                    context_window: metadata.context_window(),
                    selection: qq_protocol::ModelSelection {
                        model: Some(format!("{provider_id}/{model_id}")),
                        max_output_tokens: Some(
                            metadata
                                .max_output_tokens()
                                .map_or(snapshot.max_output_tokens(), |limit| {
                                    limit.min(snapshot.max_output_tokens())
                                }),
                        ),
                        organization: snapshot.organization().map(str::to_owned),
                    },
                });
            }
            if let Some(discovered) = discovered.get(provider_id) {
                for model in discovered {
                    if provider.models().contains_key(&model.id) {
                        continue;
                    }
                    if options.len() >= MAX_MODEL_OPTIONS {
                        break 'providers;
                    }
                    options.push(ModelDescriptor {
                        provider: provider_id.clone(),
                        model: model.id.clone(),
                        name: model.name.clone(),
                        context_window: None,
                        selection: qq_protocol::ModelSelection {
                            model: Some(format!("{provider_id}/{}", model.id)),
                            max_output_tokens: Some(snapshot.max_output_tokens()),
                            organization: snapshot.organization().map(str::to_owned),
                        },
                    });
                }
            }
        }
        if options.len() < MAX_MODEL_OPTIONS
            && !options
                .iter()
                .any(|option| option.selection.model.as_deref() == Some(snapshot.model().as_str()))
            && let Some(provider) = snapshot.providers().get(snapshot.model().provider())
            && self.provider_authenticated(snapshot.model().provider(), provider)
        {
            let metadata = provider.models().get(snapshot.model().model());
            options.push(ModelDescriptor {
                provider: snapshot.model().provider().to_owned(),
                model: snapshot.model().model().to_owned(),
                name: None,
                context_window: metadata.and_then(|metadata| metadata.context_window()),
                selection: qq_protocol::ModelSelection {
                    model: Some(snapshot.model().as_str().to_owned()),
                    max_output_tokens: Some(snapshot.max_output_tokens()),
                    organization: snapshot.organization().map(str::to_owned),
                },
            });
        }
        options.sort_by(|left, right| {
            (&left.provider, &left.name, &left.model).cmp(&(
                &right.provider,
                &right.name,
                &right.model,
            ))
        });
        options
    }

    fn discovered_model_options(&self, snapshot: &ConfigSnapshot) -> Vec<ModelDescriptor> {
        let allowed = snapshot.policy().allowed_providers();
        let denied = snapshot.policy().denied_providers();
        let mut discovered = BTreeMap::new();
        let mut attempted = 0;
        for (provider_id, provider) in snapshot.providers() {
            if allowed.is_some_and(|allowed| !allowed.iter().any(|id| id == provider_id))
                || denied.iter().any(|id| id == provider_id)
                || !self.provider_authenticated(provider_id, provider)
            {
                continue;
            }
            if attempted >= MAX_DISCOVERY_PROVIDERS {
                break;
            }
            attempted += 1;
            if let Some(models) =
                self.inner
                    .discovery
                    .discover(provider_id, provider, &self.inner.credentials)
            {
                discovered.insert(provider_id.clone(), models);
            }
        }
        self.model_options_with_discovery(snapshot, &discovered)
    }

    /// The agent profiles a workspace's configuration declares, `default`
    /// first, each with its effective model and approval mode. Blocking:
    /// loads configuration.
    pub fn profiles_for(
        &self,
        workspace: &str,
    ) -> Result<Vec<AgentProfileSummary>, RuntimeBuildError> {
        let snapshot = self.snapshot_for_selection(workspace, &ModelSelection::default())?;
        let default_route = snapshot.model().as_str().to_owned();
        let mut profiles = Vec::with_capacity(snapshot.profiles().len() + 1);
        profiles.push(AgentProfileSummary {
            id: AgentProfileId::default(),
            model: Some(default_route.clone()),
            pack: None,
            approval_mode: ApprovalMode::default(),
        });
        for (name, profile) in snapshot.profiles() {
            let id = match AgentProfileId::new(name.clone()) {
                Ok(id) => id,
                // The configuration loader validates names with the same rule;
                // a disagreement is a bug worth surfacing, not skipping.
                Err(error) => {
                    return Err(RuntimeBuildError::Config(ConfigError::InvalidProfileName(
                        format!("{name}: {error}"),
                    )));
                }
            };
            profiles.push(AgentProfileSummary {
                id,
                model: Some(
                    profile
                        .model()
                        .map_or_else(|| default_route.clone(), str::to_owned),
                ),
                pack: profile.pack().map(|reference| qq_protocol::PackSummary {
                    id: reference.pack().to_owned(),
                    version: reference.version().to_owned(),
                }),
                approval_mode: profile
                    .approval_mode()
                    .map_or(ApprovalMode::default(), |mode| match mode {
                        qq_config::ProfileApprovalMode::ReadOnly => ApprovalMode::ReadOnly,
                        qq_config::ProfileApprovalMode::Ask => ApprovalMode::Ask,
                        qq_config::ProfileApprovalMode::Auto => ApprovalMode::Auto,
                        qq_config::ProfileApprovalMode::Full => ApprovalMode::Full,
                    }),
            });
        }
        Ok(profiles)
    }

    /// The delegation roster a workspace's configuration declares, translated
    /// into the secret-free protocol shape the plan compiles with and the
    /// capability document advertises. Blocking: loads configuration.
    pub fn delegation_for(
        &self,
        workspace: &str,
    ) -> Result<qq_protocol::DelegationRoster, RuntimeBuildError> {
        let snapshot = self.snapshot_for_selection(workspace, &ModelSelection::default())?;
        Ok(delegation_roster(&snapshot, snapshot.model()))
    }

    pub fn models_for(
        &self,
        request: &ModelCatalogRequest,
    ) -> Result<Vec<ModelDescriptor>, RuntimeBuildError> {
        let requested_workspace = PathBuf::from(&request.workspace);
        let workspace = std::fs::canonicalize(&requested_workspace).map_err(|_| {
            ConfigError::InvalidWorkingDirectory {
                path: requested_workspace.clone(),
            }
        })?;
        if workspace != requested_workspace {
            return Err(ConfigError::InvalidWorkingDirectory {
                path: requested_workspace,
            }
            .into());
        }
        let mut load =
            LoadRequest::from_process_env(&workspace, request.selection.max_output_tokens)?;
        let mut overrides = load.overrides().clone();
        if let Some(model) = &request.selection.model {
            overrides = overrides.with_model(model.clone());
        }
        if let Some(organization) = &request.selection.organization {
            overrides = overrides.with_organization(organization.clone());
        }
        load = load.with_overrides(overrides);
        let snapshot = self.load(&load)?;
        Ok(self.discovered_model_options(&snapshot))
    }

    fn provider_authenticated(&self, _provider_id: &str, provider: &ProviderConfig) -> bool {
        match provider.access() {
            Some(ProviderAccess::Http(access)) => match access.auth() {
                HttpCredential::Configured(auth) => match auth {
                    ProviderAuth::NoAuth => true,
                    ProviderAuth::ApiKey(reference)
                    | ProviderAuth::Bearer(reference)
                    | ProviderAuth::Header(_, reference) => self
                        .inner
                        .credentials
                        .resolve_with_endpoint(reference, Some(access.endpoint()))
                        .is_ok(),
                },
                HttpCredential::ApiKey {
                    explicit,
                    stored_name,
                    environment_variable,
                    audience,
                } => resolve_provider_credential(
                    &self.inner.credentials,
                    explicit.as_ref(),
                    stored_name,
                    environment_variable,
                    Some(audience),
                )
                .is_ok(),
                HttpCredential::OpenAiCodex { profile } => self
                    .inner
                    .credentials
                    .resolve_with_endpoint(
                        &qq_config::SecretRef::Stored(format!(
                            "openai-codex/{}",
                            profile.as_deref().unwrap_or("default")
                        )),
                        Some("https://chatgpt.com"),
                    )
                    .is_ok(),
                HttpCredential::XAi { api_key, profile } => {
                    let profile = profile.as_deref().unwrap_or("default");
                    let stored = format!("xai/{profile}");
                    resolve_provider_credential(
                        &self.inner.credentials,
                        api_key.as_ref(),
                        &stored,
                        "XAI_API_KEY",
                        Some(qq_config::XAI_CREDENTIAL_ENDPOINT),
                    )
                    .is_ok()
                }
            },
            Some(
                ProviderAccess::AmazonBedrock { auth, .. }
                | ProviderAccess::AmazonBedrockMantle { auth, .. },
            ) => match auth {
                BedrockAuth::ApiKey(reference) => self.inner.credentials.resolve(reference).is_ok(),
                BedrockAuth::Aws(AwsAuth::Profile(profile)) => aws_profile_configured(profile),
                BedrockAuth::Aws(AwsAuth::DefaultChain) => {
                    (std::env::var_os("AWS_ACCESS_KEY_ID").is_some()
                        && std::env::var_os("AWS_SECRET_ACCESS_KEY").is_some())
                        || std::env::var_os("AWS_PROFILE")
                            .and_then(|profile| profile.into_string().ok())
                            .is_some_and(|profile| aws_profile_configured(&profile))
                        || (std::env::var_os("AWS_WEB_IDENTITY_TOKEN_FILE").is_some()
                            && std::env::var_os("AWS_ROLE_ARN").is_some())
                        || std::env::var_os("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
                        || std::env::var_os("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
                }
            },
            None => false,
        }
    }

    /// Loads the workspace configuration with `selection` applied as
    /// overrides, exactly as ordinary runtime loading would: the same
    /// canonical-workspace requirement, layering, and policy validation.
    fn snapshot_for_selection(
        &self,
        workspace: &str,
        selection: &qq_protocol::ModelSelection,
    ) -> Result<ConfigSnapshot, RuntimeBuildError> {
        let requested_workspace = PathBuf::from(workspace);
        let workspace = std::fs::canonicalize(&requested_workspace).map_err(|_| {
            ConfigError::InvalidWorkingDirectory {
                path: requested_workspace.clone(),
            }
        })?;
        if workspace != requested_workspace {
            return Err(ConfigError::InvalidWorkingDirectory {
                path: requested_workspace,
            }
            .into());
        }
        let mut load = LoadRequest::from_process_env(&workspace, selection.max_output_tokens)?;
        let mut overrides = load.overrides().clone();
        if let Some(model) = &selection.model {
            overrides = overrides.with_model(model.clone());
        }
        if let Some(organization) = &selection.organization {
            overrides = overrides.with_organization(organization.clone());
        }
        load = load.with_overrides(overrides);
        self.load(&load)
    }

    /// The spawn-time model gate: accepts exactly the routes the served
    /// model list (`POST /v1/models` and the pickers) would show right now.
    /// Route syntax, provider existence, and provider policy were already
    /// rejected by the configuration load that produced `snapshot`; this
    /// adds the authentication check that gates the served list and the
    /// model-id membership check against the builtin catalog union the
    /// cached discovery list. The default-API fallback for unknown model
    /// ids never applies on this path.
    fn validate_spawn_snapshot(&self, snapshot: &ConfigSnapshot) -> Result<(), RuntimeBuildError> {
        let provider_id = snapshot.model().provider();
        let model_id = snapshot.model().model();
        let provider = snapshot
            .providers()
            .get(provider_id)
            .ok_or_else(|| RuntimeBuildError::UnknownProvider(provider_id.to_owned()))?;
        if !self.provider_authenticated(provider_id, provider) {
            return Err(RuntimeBuildError::UnauthenticatedProvider(
                provider_id.to_owned(),
            ));
        }
        if provider.models().contains_key(model_id) {
            return Ok(());
        }
        // The discovery cache keeps this equal to the served list without a
        // network round trip while the cache is warm; when discovery is
        // unavailable the builtin catalog and configured ids above are the
        // whole list.
        if self
            .inner
            .discovery
            .discover(provider_id, provider, &self.inner.credentials)
            .is_some_and(|models| models.iter().any(|model| model.id == model_id))
        {
            return Ok(());
        }
        Err(RuntimeBuildError::UnknownModel {
            provider: provider_id.to_owned(),
            model: model_id.to_owned(),
        })
    }

    /// The blocking body of [`RuntimeLoader::validate_spawn_model`]: load
    /// (route syntax, provider existence, policy), then gate on the served
    /// model list, naming the failed check and listing the provider's
    /// routes when the list is small.
    fn validate_spawn_selection(
        &self,
        workspace: &str,
        selection: &qq_protocol::ModelSelection,
    ) -> Result<(), RuntimeLoadError> {
        let snapshot = self
            .snapshot_for_selection(workspace, selection)
            .map_err(|error| RuntimeLoadError {
                kind: error.failure_kind(),
                message: error.to_string(),
            })?;
        self.validate_spawn_snapshot(&snapshot).map_err(|error| {
            let mut message = error.to_string();
            if matches!(error, RuntimeBuildError::UnknownModel { .. })
                && let Some(routes) = self.served_route_hint(&snapshot)
            {
                message.push_str("; available routes: ");
                message.push_str(&routes);
            }
            RuntimeLoadError {
                kind: error.failure_kind(),
                message,
            }
        })
    }

    /// A short listing of the selected provider's served routes for
    /// rejection messages, omitted when the list is large or empty.
    fn served_route_hint(&self, snapshot: &ConfigSnapshot) -> Option<String> {
        const MAX_LISTED_ROUTES: usize = 12;
        let provider_id = snapshot.model().provider();
        let provider = snapshot.providers().get(provider_id)?;
        let mut ids: BTreeSet<String> = provider.models().keys().cloned().collect();
        if let Some(discovered) =
            self.inner
                .discovery
                .discover(provider_id, provider, &self.inner.credentials)
        {
            ids.extend(discovered.into_iter().map(|model| model.id));
        }
        if ids.is_empty() || ids.len() > MAX_LISTED_ROUTES {
            return None;
        }
        Some(
            ids.into_iter()
                .map(|id| format!("{provider_id}/{id}"))
                .collect::<Vec<_>>()
                .join(", "),
        )
    }

    /// Compiles (or returns the cached) plan for a load request. Blocking:
    /// performs configuration, credential, provider, and workspace work on
    /// the calling thread.
    pub fn plan_for(
        &self,
        request: &LoadRequest,
    ) -> Result<Arc<CompiledAgentPlan>, RuntimeBuildError> {
        self.plan_with_lookup(request, &AgentProfileId::default())
            .map(|(plan, _)| plan)
    }

    /// [`Self::plan_for`] under a configured agent profile. The request's
    /// overrides win over the profile, which wins over the top-level
    /// configuration.
    pub fn plan_for_profile(
        &self,
        request: &LoadRequest,
        profile: &AgentProfileId,
    ) -> Result<Arc<CompiledAgentPlan>, RuntimeBuildError> {
        self.plan_with_lookup(request, profile)
            .map(|(plan, _)| plan)
    }

    /// [`Self::plan_for`] plus how the plan was obtained, for tests and
    /// benchmarks that assert cache behavior.
    pub fn plan_with_lookup(
        &self,
        request: &LoadRequest,
        profile: &AgentProfileId,
    ) -> Result<(Arc<CompiledAgentPlan>, PlanLookup), RuntimeBuildError> {
        let workspace = qq_config::canonical_working_directory(request.cwd())?;
        let key = PlanKey {
            workspace: workspace.clone(),
            model: ModelSelection {
                model: request.overrides().model().map(str::to_owned),
                max_output_tokens: request.overrides().max_output_tokens(),
                organization: request.overrides().organization().map(str::to_owned),
            },
            profile: profile.clone(),
            explicit_config_path: request.explicit_path().map(Path::to_owned),
            explicit_config_content: request.explicit_content().map(str::to_owned),
        };
        let lookup = self.inner.plans.load(key, || {
            self.compile_generation(request, profile, &workspace)
        });
        match lookup {
            Ok(result) => Ok(result),
            Err(PlanCacheError::Compile(error)) => Err(error),
            Err(PlanCacheError::Capacity { .. }) => Err(RuntimeBuildError::PlanCacheFull),
            Err(PlanCacheError::ShutDown) => Err(RuntimeBuildError::PlanCacheShutDown),
            Err(PlanCacheError::Poisoned) => Err(RuntimeBuildError::CacheUnavailable),
        }
    }

    /// Stops the plan cache. Active runs keep the plans they hold.
    pub fn shutdown_plans(&self) {
        self.inner.plans.shutdown();
    }

    /// One full compile: configuration load, credential epoch, resolved
    /// model, provider recipe and descriptor, MCP registry, spawn routes, and
    /// the workspace itself. Also returns every filesystem source whose state
    /// decided the result so the cache can revalidate without repeating it.
    fn compile_generation(
        &self,
        request: &LoadRequest,
        profile_id: &AgentProfileId,
        workspace: &Path,
    ) -> Result<CompiledGeneration, RuntimeBuildError> {
        // The credential index is fingerprinted before secrets are read so a
        // rotation racing this compile is observed on the next lookup.
        let credential_index =
            SourceFingerprint::capture(self.inner.credentials.paths().index_file());
        let epoch = self.inner.credentials.epoch()?;
        let snapshot = self.load(request)?;
        let mut configuration_sources = vec![snapshot.sources().clone()];
        // A named profile supplies defaults beneath the request's explicit
        // overrides. Resolving it needs the merged configuration, so the load
        // repeats with the profile's values applied where the request left a
        // gap. Both loads retain their pre-read observations because the
        // first selected the profile and the second supplied runtime values.
        let selected_profile = snapshot.profile(profile_id.as_str());
        // A pack-declared profile brings its pack's resources and, when it
        // names MCP servers, restricts which declared servers join this plan.
        let pack_selection = match selected_profile.as_ref().and_then(|p| p.pack()) {
            None => None,
            Some(reference) => {
                let pack = &snapshot.packs()[reference.pack()];
                if let Some(minimum) = pack.requires().protocol
                    && minimum > qq_protocol::PROTOCOL_VERSION
                {
                    return Err(RuntimeBuildError::PackRequiresNewerProtocol {
                        pack: reference.pack().to_owned(),
                        required: minimum,
                        supported: qq_protocol::PROTOCOL_VERSION,
                    });
                }
                let profile = reference.profile();
                let relative = |path: &Path| {
                    path.strip_prefix(reference.directory())
                        .map(|p| p.to_string_lossy().into_owned())
                        .expect("pack paths were validated to stay inside the pack")
                };
                Some((
                    PackSelection {
                        id: reference.pack().to_owned(),
                        version: reference.version().to_owned(),
                        manifest_digest: reference.manifest_digest().to_owned(),
                        directory: reference.directory().to_owned(),
                        persona: profile.prompt().map(relative),
                        skill_roots: profile.skill_roots().iter().map(|p| relative(p)).collect(),
                        command_roots: profile
                            .command_roots()
                            .iter()
                            .map(|p| relative(p))
                            .collect(),
                        tool_allow: profile.tools().allow.clone(),
                        tool_deny: profile.tools().deny.clone(),
                    },
                    profile.mcp().map(<[String]>::to_vec),
                ))
            }
        };
        let snapshot = match selected_profile {
            None => return Err(RuntimeBuildError::UnknownProfile(profile_id.clone())),
            Some(profile) if profile == qq_config::AgentProfileConfig::default() => snapshot,
            Some(profile) => {
                let mut overrides = request.overrides().clone();
                if request.overrides().model().is_none()
                    && let Some(model) = profile.model()
                {
                    overrides = overrides.with_model(model.to_owned());
                }
                if request.overrides().organization().is_none()
                    && let Some(organization) = profile.organization()
                {
                    overrides = overrides.with_organization(organization.to_owned());
                }
                if request.overrides().max_output_tokens().is_none()
                    && let Some(cap) = profile.max_output_tokens()
                {
                    overrides = overrides.with_max_output_tokens(cap);
                }
                let snapshot = self.load(&request.clone().with_overrides(overrides))?;
                if !configuration_sources.contains(snapshot.sources()) {
                    configuration_sources.push(snapshot.sources().clone());
                }
                snapshot
            }
        };
        let resolved_model = self.resolved_model_for_snapshot(&snapshot)?;
        let provider_id = snapshot.model().provider();
        let provider_config = snapshot
            .providers()
            .get(provider_id)
            .ok_or_else(|| RuntimeBuildError::UnknownProvider(provider_id.to_owned()))?;
        let (recipe, descriptor) =
            self.prepare_provider(provider_id, snapshot.model().model(), provider_config)?;
        let provider = self.inner.providers.compile(recipe)?;
        let spawn_model_routes = self
            .configured_model_options(&snapshot)
            .into_iter()
            .filter_map(|model| model.selection.model)
            .collect();
        let provenance = snapshot
            .source_reports()
            .iter()
            .map(|report| report.source().label().to_owned())
            .collect();
        let delegation = delegation_roster(&snapshot, snapshot.model());
        let audit = audit_policy(snapshot.audit());
        let mut profile =
            AgentProfile::new(provider, descriptor, resolved_model, workspace.to_owned())
                .with_spawn_model_routes(spawn_model_routes)
                .with_delegation(delegation)
                .with_audit(audit)
                .with_provenance(provenance)
                .with_credential_epoch(epoch)
                .with_profile_id(profile_id.clone());
        let mcp_subset = match pack_selection {
            Some((selection, subset)) => {
                profile = profile.with_pack(selection);
                subset
            }
            None => None,
        };
        if let Some(names) = snapshot.policy().exposed_tools() {
            // A profile-excluded configured server is outside discovery.
            // Unknown namespaces stay in the list and fail core validation.
            let names = names
                .iter()
                .filter(|name| {
                    let server = name
                        .strip_prefix("mcp__")
                        .and_then(|rest| rest.split_once("__"))
                        .map(|(server, _)| server);
                    !server.is_some_and(|server| {
                        snapshot.mcp_servers().contains_key(server)
                            && mcp_subset
                                .as_ref()
                                .is_some_and(|subset| !subset.iter().any(|name| name == server))
                    })
                })
                .cloned()
                .collect();
            profile = profile.with_exposed_tools(names);
        }
        let mut bindings = LiveBindings {
            provider: provider_config.access().cloned(),
            mcp: None,
        };
        if let Some(wired) = self.inner.mcp.registry_for_snapshot(
            &self.inner.credentials,
            epoch,
            &snapshot,
            mcp_subset.as_deref(),
        )? {
            // The catalog is snapshotted here, on the blocking compile
            // thread, so the plan holds an immutable tool list and the cache
            // can revalidate it against the manager's generation.
            bindings.mcp = Some(Arc::clone(&wired.registry));
            profile = profile
                .with_host(HostSnapshot::capture_blocking(wired.registry))
                .with_mcp_servers(wired.servers);
        }
        let plan = CompiledAgentPlan::compile_blocking(profile)?;
        let mut sources = Vec::with_capacity(plan.instruction_sources().len() + 1);
        sources.push(credential_index);
        sources.extend(plan.instruction_sources().iter().cloned());
        Ok(CompiledGeneration {
            plan,
            sources,
            configuration_sources,
            bindings,
        })
    }

    fn resolved_model_for_snapshot(
        &self,
        snapshot: &ConfigSnapshot,
    ) -> Result<ResolvedModel, RuntimeBuildError> {
        let provider_id = snapshot.model().provider();
        let provider = snapshot
            .providers()
            .get(provider_id)
            .ok_or_else(|| RuntimeBuildError::UnknownProvider(provider_id.to_owned()))?;
        let access = provider
            .access()
            .ok_or_else(|| RuntimeBuildError::IncompleteProvider(provider_id.to_owned()))?;
        let metadata = provider.models().get(snapshot.model().model());
        let max_output_tokens = metadata
            .and_then(qq_config::ModelMetadata::max_output_tokens)
            .map_or(snapshot.max_output_tokens(), |model_limit| {
                model_limit.min(snapshot.max_output_tokens())
            });
        let api = effective_provider_api(provider, snapshot.model().model(), access);
        if matches!(
            api,
            ProviderApi::GoogleGenerateContent | ProviderApi::BedrockConverse
        ) && max_output_tokens > i32::MAX as u32
        {
            return Err(RuntimeBuildError::UnrepresentableOutputLimit {
                provider: provider_id.to_owned(),
                model: snapshot.model().model().to_owned(),
                limit: max_output_tokens,
            });
        }
        let codex = matches!(
            access,
            ProviderAccess::Http(access)
                if matches!(access.auth(), HttpCredential::OpenAiCodex { .. })
        );
        let (cache_read_usage, cache_write_usage) = match api {
            ProviderApi::OpenAiResponses
            | ProviderApi::OpenAiChatCompletions
            | ProviderApi::GoogleGenerateContent => (true, false),
            ProviderApi::AnthropicMessages | ProviderApi::BedrockConverse => (true, true),
        };
        let credential_profile = match access {
            ProviderAccess::Http(access) => match access.auth() {
                HttpCredential::OpenAiCodex { profile } | HttpCredential::XAi { profile, .. } => {
                    Some(profile.as_deref().unwrap_or("default").to_owned())
                }
                HttpCredential::Configured(_) | HttpCredential::ApiKey { .. } => None,
            },
            ProviderAccess::AmazonBedrock { auth, .. }
            | ProviderAccess::AmazonBedrockMantle { auth, .. } => match auth {
                BedrockAuth::Aws(AwsAuth::Profile(profile)) => Some(profile.clone()),
                BedrockAuth::Aws(AwsAuth::DefaultChain) | BedrockAuth::ApiKey(_) => None,
            },
        };
        Ok(ResolvedModel {
            version: ResolvedModelVersion::new(2)
                .expect("resolved-model schema version must be non-zero"),
            request_shape: provider_request_shape_identity(provider_id, provider, access, api),
            route: snapshot.model().as_str().to_owned(),
            provider_model: snapshot.model().model().to_owned(),
            organization: snapshot.organization().map(str::to_owned),
            credential_profile,
            max_output_tokens,
            context_window: metadata.and_then(qq_config::ModelMetadata::context_window),
            pricing: metadata
                .and_then(qq_config::ModelMetadata::pricing)
                .cloned()
                .map(protocol_model_pricing),
            output_token_control: if codex {
                CapabilitySupport::Unsupported
            } else {
                CapabilitySupport::Native
            },
            generation: GenerationCapabilities {
                reasoning_effort: CapabilitySupport::Unsupported,
            },
            prompt_cache: PromptCacheCapabilities {
                control: CapabilitySupport::Unsupported,
                cache_read_usage,
                cache_write_usage,
            },
        })
    }

    fn prepare_provider(
        &self,
        provider_id: &str,
        model_id: &str,
        config: &ProviderConfig,
    ) -> Result<(ProviderRecipe, ProviderDescriptor), RuntimeBuildError> {
        let access = config
            .access()
            .ok_or_else(|| RuntimeBuildError::IncompleteProvider(provider_id.to_owned()))?;
        let api = effective_provider_api(config, model_id, access);
        match access {
            ProviderAccess::Http(access) => self.prepare_http_provider(provider_id, access, api),
            ProviderAccess::AmazonBedrock { region, auth } => {
                self.prepare_bedrock_provider(provider_id, region.as_deref(), auth)
            }
            ProviderAccess::AmazonBedrockMantle { region, auth, .. } => {
                self.prepare_bedrock_mantle_provider(provider_id, region.as_deref(), api, auth)
            }
        }
    }

    fn prepare_http_provider(
        &self,
        provider_id: &str,
        access: &HttpAccess,
        api: ProviderApi,
    ) -> Result<(ProviderRecipe, ProviderDescriptor), RuntimeBuildError> {
        let (auth, auth_scheme, credential) = match access.auth() {
            HttpCredential::Configured(auth) => {
                let (scheme, credential) = match auth {
                    ProviderAuth::NoAuth => ("none".to_owned(), CredentialReference::None),
                    ProviderAuth::ApiKey(reference) => {
                        ("api_key".to_owned(), credential_reference(reference))
                    }
                    ProviderAuth::Bearer(reference) => {
                        ("bearer".to_owned(), credential_reference(reference))
                    }
                    ProviderAuth::Header(name, reference) => {
                        (format!("header:{name}"), credential_reference(reference))
                    }
                };
                (
                    self.resolve_http_auth(auth, access.endpoint())?
                        .into_http()?,
                    scheme,
                    credential,
                )
            }
            HttpCredential::ApiKey {
                explicit,
                stored_name,
                environment_variable,
                audience,
            } => {
                let secret = resolve_provider_credential(
                    &self.inner.credentials,
                    explicit.as_ref(),
                    stored_name,
                    environment_variable,
                    Some(audience),
                )?;
                // Built-in providers try the explicit reference, then the
                // stored name, then the environment variable; the descriptor
                // names the source that actually applies.
                let credential = match explicit {
                    Some(reference) => credential_reference(reference),
                    None => {
                        if self.inner.credentials.is_registered(stored_name)? {
                            CredentialReference::Stored((*stored_name).to_owned())
                        } else {
                            CredentialReference::Environment(environment_variable.to_string())
                        }
                    }
                };
                (
                    ResolvedAuth::ApiKey(secret).into_http()?,
                    "api_key".to_owned(),
                    credential,
                )
            }
            HttpCredential::OpenAiCodex { profile } => {
                let profile = profile.as_deref().unwrap_or("default");
                (
                    HttpAuth::RequestTimeCodex(
                        self.inner.credentials.codex_request_credentials(profile),
                    ),
                    "codex".to_owned(),
                    CredentialReference::Profile(profile.to_owned()),
                )
            }
            HttpCredential::XAi { api_key, profile } => {
                let profile = profile.as_deref().unwrap_or("default");
                (
                    HttpAuth::RequestTimeBearer(
                        self.inner
                            .credentials
                            .xai_request_credentials(profile, api_key.clone()),
                    ),
                    "bearer".to_owned(),
                    CredentialReference::Profile(profile.to_owned()),
                )
            }
        };
        let headers = access
            .headers()
            .iter()
            .map(|(name, value)| (name.clone(), value.expose_value().to_owned()))
            .collect::<Vec<_>>();
        let endpoint_mode = match access.endpoint_mode() {
            EndpointMode::Base => "base",
            EndpointMode::Exact => "exact",
        };
        let descriptor = ProviderDescriptor {
            id: provider_id.to_owned(),
            api: provider_api_name(api).to_owned(),
            endpoint: Some(describe_endpoint(access.endpoint())),
            endpoint_mode: Some(endpoint_mode.to_owned()),
            auth_scheme,
            credential,
            header_names: access.headers().keys().cloned().collect(),
            region: None,
        };
        let protocol = http_protocol(provider_id, api)?;
        let allow_http = access
            .endpoint()
            .split_once("://")
            .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("http"));
        let endpoint = match access.endpoint_mode() {
            EndpointMode::Base => EndpointSpec::base(access.endpoint(), allow_http),
            EndpointMode::Exact => EndpointSpec::exact(access.endpoint(), allow_http),
        };
        let recipe = ProviderRecipe::http(
            HttpProviderRecipe::new(endpoint, protocol, auth).with_headers(headers),
        );
        Ok((recipe, descriptor))
    }

    fn prepare_bedrock_provider(
        &self,
        provider_id: &str,
        region: Option<&str>,
        auth: &BedrockAuth,
    ) -> Result<(ProviderRecipe, ProviderDescriptor), RuntimeBuildError> {
        let credential_endpoint =
            region.map(|region| format!("https://bedrock-runtime.{region}.amazonaws.com"));
        let (auth, auth_scheme, credential) =
            self.prepare_bedrock_auth(auth, credential_endpoint.as_deref())?;
        let descriptor = ProviderDescriptor {
            id: provider_id.to_owned(),
            api: "bedrock_converse".to_owned(),
            endpoint: None,
            endpoint_mode: None,
            auth_scheme,
            credential,
            header_names: Vec::new(),
            region: region.map(str::to_owned),
        };
        Ok((
            ProviderRecipe::amazon_bedrock(region.map(str::to_owned), auth),
            descriptor,
        ))
    }

    fn prepare_bedrock_mantle_provider(
        &self,
        provider_id: &str,
        region: Option<&str>,
        api: ProviderApi,
        auth: &BedrockAuth,
    ) -> Result<(ProviderRecipe, ProviderDescriptor), RuntimeBuildError> {
        let protocol = match api {
            ProviderApi::OpenAiResponses => HttpProtocol::OpenAiResponses,
            ProviderApi::OpenAiChatCompletions => HttpProtocol::OpenAiChatCompletions,
            ProviderApi::AnthropicMessages => HttpProtocol::AnthropicMessages,
            api => {
                return Err(RuntimeBuildError::UnsupportedApi {
                    provider: provider_id.to_owned(),
                    api,
                });
            }
        };
        let credential_endpoint =
            region.map(|region| format!("https://bedrock-mantle.{region}.api.aws"));
        let (auth, auth_scheme, credential) =
            self.prepare_bedrock_auth(auth, credential_endpoint.as_deref())?;
        let descriptor = ProviderDescriptor {
            id: provider_id.to_owned(),
            api: provider_api_name(api).to_owned(),
            endpoint: None,
            endpoint_mode: None,
            auth_scheme,
            credential,
            header_names: Vec::new(),
            region: region.map(str::to_owned),
        };
        Ok((
            ProviderRecipe::amazon_bedrock_mantle(region.map(str::to_owned), protocol, auth),
            descriptor,
        ))
    }

    fn prepare_bedrock_auth(
        &self,
        auth: &BedrockAuth,
        credential_endpoint: Option<&str>,
    ) -> Result<(ProviderBedrockAuth, String, CredentialReference), RuntimeBuildError> {
        Ok(match auth {
            BedrockAuth::Aws(AwsAuth::DefaultChain) => (
                ProviderBedrockAuth::DefaultChain,
                "sigv4".to_owned(),
                CredentialReference::AmbientChain,
            ),
            BedrockAuth::Aws(AwsAuth::Profile(profile)) => (
                ProviderBedrockAuth::Profile(profile.clone()),
                "sigv4".to_owned(),
                CredentialReference::Profile(profile.clone()),
            ),
            BedrockAuth::ApiKey(reference) => {
                let secret = self
                    .inner
                    .credentials
                    .resolve_with_endpoint(reference, credential_endpoint)?;
                (
                    ProviderBedrockAuth::ApiKey(secret.expose_secret_str()?.to_owned().into()),
                    "api_key".to_owned(),
                    credential_reference(reference),
                )
            }
        })
    }

    fn resolve_http_auth(
        &self,
        auth: &ProviderAuth,
        endpoint: &str,
    ) -> Result<ResolvedAuth, RuntimeBuildError> {
        match auth {
            ProviderAuth::NoAuth => Ok(ResolvedAuth::NoAuth),
            ProviderAuth::ApiKey(reference) => {
                let secret = self
                    .inner
                    .credentials
                    .resolve_with_endpoint(reference, Some(endpoint))?;
                Ok(ResolvedAuth::ApiKey(secret))
            }
            ProviderAuth::Bearer(reference) => {
                let secret = self
                    .inner
                    .credentials
                    .resolve_with_endpoint(reference, Some(endpoint))?;
                Ok(ResolvedAuth::Bearer(secret))
            }
            ProviderAuth::Header(name, reference) => {
                let secret = self
                    .inner
                    .credentials
                    .resolve_with_endpoint(reference, Some(endpoint))?;
                Ok(ResolvedAuth::Header(name.clone(), secret))
            }
        }
    }
}

fn effective_provider_api(
    provider: &ProviderConfig,
    model: &str,
    access: &ProviderAccess,
) -> ProviderApi {
    let model_api = provider
        .models()
        .get(model)
        .and_then(qq_config::ModelMetadata::api);
    match access {
        ProviderAccess::Http(access) => model_api.unwrap_or(access.api()),
        ProviderAccess::AmazonBedrock { .. } => ProviderApi::BedrockConverse,
        ProviderAccess::AmazonBedrockMantle { api, .. } => model_api.unwrap_or(*api),
    }
}

fn aws_profile_configured(profile: &str) -> bool {
    if profile.is_empty() {
        return false;
    }
    let home = directories::BaseDirs::new().map(|directories| directories.home_dir().to_owned());
    let files = [
        std::env::var_os("AWS_CONFIG_FILE")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".aws/config"))),
        std::env::var_os("AWS_SHARED_CREDENTIALS_FILE")
            .map(PathBuf::from)
            .or_else(|| home.map(|home| home.join(".aws/credentials"))),
    ];
    let config_header = format!("[profile {profile}]");
    let credentials_header = format!("[{profile}]");
    files.into_iter().flatten().any(|path| {
        std::fs::read_to_string(path).is_ok_and(|content| {
            content.lines().any(|line| {
                let line = line.trim();
                line == config_header || line == credentials_header
            })
        })
    })
}

impl RuntimeLoader for RuntimeFactory {
    fn resolve_worker_model(
        &self,
        workspace: String,
        parent: qq_protocol::ModelSelection,
    ) -> WorkerRuntimeLoadFuture {
        let factory = self.clone();
        Box::pin(async move {
            let build = tokio::task::spawn_blocking(move || {
                let snapshot = factory.snapshot_for_selection(&workspace, &parent)?;
                Ok::<_, RuntimeBuildError>(match snapshot.worker_model() {
                    Some(worker) => qq_protocol::ModelSelection {
                        model: Some(worker.as_str().to_owned()),
                        max_output_tokens: Some(snapshot.max_output_tokens()),
                        organization: snapshot.organization().map(str::to_owned),
                    },
                    None => parent,
                })
            })
            .await;
            match build {
                Ok(Ok(selection)) => Ok(selection),
                Ok(Err(error)) => Err(RuntimeLoadError {
                    kind: error.failure_kind(),
                    message: error.to_string(),
                }),
                Err(_) => Err(RuntimeLoadError {
                    kind: RunFailureKind::Server,
                    message: "worker model resolution stopped unexpectedly".to_owned(),
                }),
            }
        })
    }

    fn validate_spawn_model(
        &self,
        workspace: String,
        selection: qq_protocol::ModelSelection,
    ) -> SpawnModelValidationFuture {
        let factory = self.clone();
        Box::pin(async move {
            let build = tokio::task::spawn_blocking(move || {
                factory.validate_spawn_selection(&workspace, &selection)
            })
            .await;
            match build {
                Ok(result) => result,
                Err(_) => Err(RuntimeLoadError {
                    kind: RunFailureKind::Server,
                    message: "spawn model validation stopped unexpectedly".to_owned(),
                }),
            }
        })
    }

    fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
        let factory = self.clone();
        Box::pin(async move {
            let build = tokio::task::spawn_blocking(move || {
                let requested_workspace = PathBuf::from(&request.workspace);
                let workspace = std::fs::canonicalize(&requested_workspace).map_err(|_| {
                    ConfigError::InvalidWorkingDirectory {
                        path: requested_workspace.clone(),
                    }
                })?;
                if workspace != requested_workspace {
                    return Err(ConfigError::InvalidWorkingDirectory {
                        path: requested_workspace,
                    }
                    .into());
                }
                let mut load =
                    LoadRequest::from_process_env(&workspace, request.model.max_output_tokens)?;
                let mut overrides = load.overrides().clone();
                if let Some(model) = request.model.model {
                    overrides = overrides.with_model(model);
                }
                if let Some(organization) = request.model.organization {
                    overrides = overrides.with_organization(organization);
                }
                load = load.with_overrides(overrides);
                let plan = factory.plan_for_profile(&load, &request.profile)?;
                Ok::<_, RuntimeBuildError>(LoadedRuntime { plan })
            })
            .await;
            match build {
                Ok(Ok(runtime)) => Ok(runtime),
                Ok(Err(error)) => Err(RuntimeLoadError {
                    kind: error.failure_kind(),
                    message: error.to_string(),
                }),
                Err(_) => Err(RuntimeLoadError {
                    kind: RunFailureKind::Server,
                    message: "runtime construction stopped unexpectedly".to_owned(),
                }),
            }
        })
    }
}

impl WorkspaceGrantAuthority for RuntimeFactory {
    fn seed_grants(&self, workspace: &Path) -> GrantSeedFuture {
        let factory = self.clone();
        let workspace = workspace.to_owned();
        Box::pin(async move {
            let seed = tokio::task::spawn_blocking(move || {
                let load = LoadRequest::from_process_env(&workspace, None).ok()?;
                let snapshot = factory.load(&load).ok()?;
                let grants = snapshot.grants();
                Some(WorkspaceGrantSeed {
                    tools: grants.tools().to_vec(),
                    shell_prefixes: grants.shell_prefixes().to_vec(),
                })
            })
            .await;
            // A configuration that fails to load seeds nothing: the session
            // is still created, and the next run surfaces the configuration
            // error through the ordinary run-failure path.
            seed.ok().flatten().unwrap_or_default()
        })
    }

    fn promote_grant(&self, workspace: &Path, grant: &ApprovalGrant) -> GrantPromotionFuture {
        let factory = self.clone();
        let workspace = workspace.to_owned();
        let grant = match grant {
            ApprovalGrant::Tool { name } => WorkspaceGrant::Tool(name.clone()),
            ApprovalGrant::ShellPrefix { prefix } => WorkspaceGrant::ShellPrefix(prefix.clone()),
        };
        Box::pin(async move {
            let config = factory.inner.config.clone();
            let written = tokio::task::spawn_blocking(move || {
                config.promote_workspace_grant(&workspace, &grant)
            })
            .await;
            match written {
                Ok(Ok(promotion)) => {
                    let path = promotion.path().display().to_string();
                    match promotion.outcome() {
                        PromotionOutcome::Added => WorkspaceGrantOutcome::Written { path },
                        PromotionOutcome::AlreadyPresent => {
                            WorkspaceGrantOutcome::AlreadyPresent { path }
                        }
                    }
                }
                Ok(Err(error)) => WorkspaceGrantOutcome::Failed {
                    message: error.to_string(),
                },
                Err(_) => WorkspaceGrantOutcome::Failed {
                    message: "the workspace grant write stopped unexpectedly".to_owned(),
                },
            }
        })
    }
}

/// One-shot reviewer verdict budget: a bounded read of a small model's single
/// JSON line. Raised from 512 when requests gained arguments and briefs.
const REVIEWER_MAX_OUTPUT_TOKENS: u32 = 1_024;
const REVIEWER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Bytes of one section (arguments, brief, diff) quoted to the reviewer.
const REVIEWER_SECTION_BYTES: usize = 8 * 1024;

/// Adjudicates held tool approvals with the workspace-configured
/// `reviewer_model`, through the same provider compilation path as ordinary
/// runs. Every failure — no reviewer configured, config or provider errors,
/// timeout, unparseable verdict — resolves as `Escalate`, leaving the human
/// approval path untouched.
///
/// The compiled provider is cached per workspace and revalidated by
/// credential epoch, so a held call does not pay a configuration load.
pub struct ModelApprovalReviewer {
    factory: RuntimeFactory,
    cache: Arc<std::sync::Mutex<HashMap<PathBuf, CachedReviewer>>>,
}

#[derive(Clone)]
struct CachedReviewer {
    epoch: qq_protocol::CredentialEpoch,
    route: qq_config::ModelRoute,
    provider: Arc<dyn qq_provider::Provider>,
    pricing: Option<qq_protocol::ModelPricing>,
}

impl ModelApprovalReviewer {
    pub fn new(factory: RuntimeFactory) -> Self {
        Self {
            factory,
            cache: Arc::new(std::sync::Mutex::new(HashMap::new())),
        }
    }

    fn escalate(reason: &str) -> ReviewVerdict {
        ReviewVerdict::free(ReviewDecision::Escalate {
            reason: reason.to_owned(),
        })
    }

    /// Blocking: the cached reviewer for `workspace` when its credential epoch
    /// still matches, else a fresh compile. A rotated credential or changed
    /// reviewer route is observed on the next epoch.
    fn prepare(&self, workspace: &Path) -> Result<CachedReviewer, String> {
        let epoch = self
            .factory
            .inner
            .credentials
            .epoch()
            .map_err(|error| error.to_string())?;
        if let Ok(cache) = self.cache.lock()
            && let Some(cached) = cache.get(workspace)
            && cached.epoch == epoch
        {
            return Ok(cached.clone());
        }
        let load =
            LoadRequest::from_process_env(workspace, None).map_err(|error| error.to_string())?;
        let snapshot = self
            .factory
            .load(&load)
            .map_err(|error| error.to_string())?;
        let Some(route) = snapshot.reviewer_model() else {
            return Err("no reviewer model is configured".to_owned());
        };
        let provider_config = snapshot
            .providers()
            .get(route.provider())
            .ok_or_else(|| format!("unknown reviewer provider {}", route.provider()))?;
        let (recipe, _) = self
            .factory
            .prepare_provider(route.provider(), route.model(), provider_config)
            .map_err(|error| error.to_string())?;
        let provider = self
            .factory
            .inner
            .providers
            .compile(recipe)
            .map_err(|error| error.to_string())?;
        let pricing = provider_config
            .models()
            .get(route.model())
            .and_then(qq_config::ModelMetadata::pricing)
            .cloned()
            .map(protocol_model_pricing);
        let cached = CachedReviewer {
            epoch,
            route: route.clone(),
            provider,
            pricing,
        };
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(workspace.to_owned(), cached.clone());
        }
        Ok(cached)
    }
}

impl ApprovalReviewer for ModelApprovalReviewer {
    fn review(&self, request: ReviewRequest) -> ReviewFuture {
        let reviewer = Self {
            factory: self.factory.clone(),
            cache: Arc::clone(&self.cache),
        };
        Box::pin(async move {
            let workspace = PathBuf::from(&request.workspace);
            let prepared = tokio::task::spawn_blocking(move || reviewer.prepare(&workspace)).await;
            let cached = match prepared {
                Ok(Ok(prepared)) => prepared,
                Ok(Err(_)) | Err(_) => return Self::escalate("reviewer unavailable"),
            };
            let verdict = tokio::time::timeout(
                REVIEWER_TIMEOUT,
                collect_reviewer_verdict(
                    cached.provider,
                    cached.route.model(),
                    cached.pricing.as_ref(),
                    &request,
                ),
            )
            .await;
            match verdict {
                Ok(verdict) => verdict,
                Err(_) => Self::escalate("reviewer timed out"),
            }
        })
    }
}

/// The reviewer's contract: one strict JSON line. Anything else escalates.
const REVIEWER_SYSTEM_PROMPT: &str = "You are a tool-call reviewer for a coding agent. \
The agent proposed one action that policy held for review. Judge only the action shown; \
you have no transcript and must not assume intent beyond the stated task. \
Two criteria, both required to approve: (1) safety — the worst plausible outcome is \
recoverable within the workspace (for example: deleting build artifacts, killing a process \
the workspace owns, reinstalling dependencies, editing source files); escalate anything \
externally visible, credential-touching, system-level, or ambiguous (force-pushing shared \
branches, sudo, piped installers, writes outside the workspace). (2) necessity — when a task \
brief is given, the action must be plausibly necessary for that task; deny actions clearly \
outside it. For a supervised sub-agent your deny is final; for a root session it only \
escalates to the human. \
Reply with exactly one JSON object on one line and nothing else: \
{\"verdict\":\"approve\"} or {\"verdict\":\"escalate\",\"reason\":\"...\"} \
or {\"verdict\":\"deny\",\"reason\":\"...\"}.";

async fn collect_reviewer_verdict(
    provider: Arc<dyn qq_provider::Provider>,
    model: &str,
    pricing: Option<&qq_protocol::ModelPricing>,
    request: &ReviewRequest,
) -> ReviewVerdict {
    use futures_util::StreamExt as _;
    use qq_provider::{ContentBlock, Message, ModelRequest, ProviderEvent, Role};

    let mut description = format!(
        "Tool: {}\nWorkspace: {}\nSession mode: {}\n",
        request.tool_name,
        request.workspace,
        match request.mode {
            qq_protocol::ApprovalMode::Supervised => "supervised sub-agent (your deny is final)",
            _ => "root session (deny escalates to the human)",
        }
    );
    match request.origin {
        qq_core::ReviewOrigin::Root => {}
        qq_core::ReviewOrigin::Child { depth, .. } => {
            description.push_str(&format!("Origin: sub-agent at depth {depth}\n"));
        }
    }
    if let Some(brief) = &request.task_brief {
        description.push_str("Task brief given to the sub-agent:\n");
        description.push_str(&bounded(brief, REVIEWER_SECTION_BYTES));
        description.push('\n');
    }
    if let Some(shell) = &request.shell {
        description.push_str("Shell command: ");
        description.push_str(&shell.command);
        description.push('\n');
        if let Some(cwd) = &shell.cwd {
            description.push_str("Working directory: ");
            description.push_str(cwd);
            description.push('\n');
        }
    }
    if let Some(edit) = &request.edit {
        description.push_str("Edit path: ");
        description.push_str(&edit.path);
        description.push_str("\nDiff preview:\n");
        description.push_str(&bounded(&edit.diff, REVIEWER_SECTION_BYTES));
        description.push('\n');
    }
    if request.shell.is_none() && request.edit.is_none() {
        description.push_str("Arguments: ");
        description.push_str(&bounded(&request.arguments, REVIEWER_SECTION_BYTES));
        description.push('\n');
    }
    if !request.recent_actions.is_empty() {
        description.push_str("Recent tool calls of this run (oldest first): ");
        let listed = request
            .recent_actions
            .iter()
            .map(|action| match &action.path {
                Some(path) => format!("{}({})", action.tool, bounded(path, 120)),
                None => action.tool.clone(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        description.push_str(&listed);
        description.push('\n');
    }
    if !request.granted_tools.is_empty() || !request.granted_shell_prefixes.is_empty() {
        description.push_str(&format!(
            "Session grants: tools {:?}; shell prefixes {:?}\n",
            request.granted_tools, request.granted_shell_prefixes
        ));
    }
    let model_request = ModelRequest::new(
        model.to_owned(),
        vec![Message::new(
            Role::User,
            vec![ContentBlock::Text { text: description }],
        )],
        REVIEWER_MAX_OUTPUT_TOKENS,
    )
    .with_system(REVIEWER_SYSTEM_PROMPT);
    let mut stream = provider.stream(model_request);
    let mut text = String::new();
    let mut spend = qq_core::ReviewSpend::default();
    loop {
        match stream.next().await {
            Some(Ok(ProviderEvent::OutputTextDelta { text: delta })) => text.push_str(&delta),
            Some(Ok(
                ProviderEvent::Completed { usage } | ProviderEvent::Incomplete { usage, .. },
            )) => {
                let usage = usage.map(|usage| qq_protocol::TokenUsage {
                    input_tokens: usage.input_tokens,
                    cache_read_input_tokens: usage.cache_read_input_tokens,
                    cache_write_input_tokens: usage.cache_write_input_tokens,
                    output_tokens: usage.output_tokens,
                    reasoning_tokens: usage.reasoning_tokens,
                });
                spend = qq_core::ReviewSpend {
                    usage,
                    cost_usd_nanos: match (usage, pricing) {
                        (Some(usage), Some(pricing)) => qq_core::run_cost(usage, pricing),
                        _ => None,
                    },
                };
                break;
            }
            Some(Ok(_)) => {}
            Some(Err(_)) => {
                return ReviewVerdict {
                    decision: ReviewDecision::Escalate {
                        reason: "reviewer request failed".to_owned(),
                    },
                    // The request was sent; its spend is unknown, not zero.
                    spend: qq_core::ReviewSpend {
                        usage: None,
                        cost_usd_nanos: None,
                    },
                };
            }
            None => break,
        }
    }
    ReviewVerdict {
        decision: parse_reviewer_decision(&text),
        spend,
    }
}

fn bounded(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &text[..end])
}

/// Parses the reviewer's reply. The verdict must be the only JSON object in
/// the reply and `approve` carries no qualifier; everything else escalates.
fn parse_reviewer_decision(text: &str) -> ReviewDecision {
    #[derive(serde::Deserialize)]
    struct Reply {
        verdict: String,
        #[serde(default)]
        reason: Option<String>,
    }
    let trimmed = text.trim();
    let Ok(reply) = serde_json::from_str::<Reply>(trimmed) else {
        return ReviewDecision::Escalate {
            reason: "reviewer reply was not a valid verdict".to_owned(),
        };
    };
    let reason = |reply: Reply| {
        reply
            .reason
            .unwrap_or_else(|| "reviewer verdict".to_owned())
    };
    match reply.verdict.as_str() {
        "approve" => ReviewDecision::Approve,
        "deny" => ReviewDecision::Deny {
            reason: reason(reply),
        },
        _ => ReviewDecision::Escalate {
            reason: reason(reply),
        },
    }
}

enum ResolvedAuth {
    NoAuth,
    ApiKey(Secret),
    Bearer(Secret),
    Header(String, Secret),
}

/// The secret-free reference a configured secret resolves through.
fn credential_reference(reference: &SecretRef) -> CredentialReference {
    match reference {
        SecretRef::Env(name) => CredentialReference::Environment(name.clone()),
        SecretRef::Stored(name) => CredentialReference::Stored(name.clone()),
        SecretRef::Value(_) => CredentialReference::Inline,
    }
}

/// An endpoint reduced to scheme, host, port, and path. Userinfo, query, and
/// fragment can carry credentials and are dropped; an unparseable endpoint is
/// described only by its scheme so no raw bytes leak into a descriptor.
pub(crate) fn describe_endpoint(endpoint: &str) -> String {
    match reqwest::Url::parse(endpoint) {
        Ok(url) => {
            let mut described = format!("{}://", url.scheme());
            if let Some(host) = url.host_str() {
                described.push_str(host);
            }
            if let Some(port) = url.port() {
                described.push(':');
                described.push_str(&port.to_string());
            }
            described.push_str(url.path());
            described
        }
        Err(_) => endpoint.split_once("://").map_or_else(
            || "unparseable".to_owned(),
            |(scheme, _)| format!("{scheme}://<unparseable>"),
        ),
    }
}

impl ResolvedAuth {
    fn into_http(self) -> Result<HttpAuth, AuthError> {
        match self {
            Self::NoAuth => Ok(HttpAuth::NoAuth),
            Self::ApiKey(secret) => Ok(HttpAuth::ApiKey(secret.expose_secret_str()?.into())),
            Self::Bearer(secret) => Ok(HttpAuth::Bearer(secret.expose_secret_str()?.into())),
            Self::Header(name, secret) => {
                Ok(HttpAuth::Header(name, secret.expose_secret_str()?.into()))
            }
        }
    }
}

#[derive(Clone)]
pub struct RuntimeHandler {
    durable: SessionRuntime,
    factory: RuntimeFactory,
}

impl RuntimeHandler {
    pub async fn open(factory: RuntimeFactory) -> Result<Self, RuntimeHandlerError> {
        let database_path = factory.inner.config.session_database_path()?;
        // The factory is both the runtime loader and the workspace grant
        // authority: config grants seed each new session's grant set, and
        // approve-for-workspace promotions write back through the loader's
        // configuration layer.
        let options = SessionRuntimeOptions::new(database_path)
            .with_grant_authority(Arc::new(factory.clone()))
            .with_approval_reviewer(Arc::new(ModelApprovalReviewer::new(factory.clone())));
        let durable = SessionRuntime::open(options, Arc::new(factory.clone())).await?;
        Ok(Self { durable, factory })
    }

    /// The durable session runtime this handler serves. Headless `qq run`
    /// drives it directly through the same command/snapshot/subscribe
    /// interface the server exposes over HTTP.
    pub fn sessions(&self) -> &SessionRuntime {
        &self.durable
    }

    /// Gracefully stops the durable runtime after its serving adapter has
    /// stopped accepting new requests.
    pub async fn shutdown(&self) -> Result<(), RuntimeHandlerError> {
        // Runs settle first so every active plan is released before the
        // cache stops admitting; a plan a run still holds stays valid.
        self.durable.shutdown().await?;
        self.factory.shutdown_plans();
        Ok(())
    }

    /// Finalizes the runtime after every serving adapter has drained its
    /// snapshot and event responses.
    pub async fn close(&self) -> Result<(), RuntimeHandlerError> {
        self.durable.close().await?;
        Ok(())
    }
}

/// The capability document's workspace tool section, read off a compiled
/// plan: nothing is fetched, so a warm plan answers from memory.
fn workspace_tool_capabilities(plan: &CompiledAgentPlan) -> qq_protocol::WorkspaceToolCapabilities {
    let catalog = plan.catalog();
    let skills = plan.skills();
    qq_protocol::WorkspaceToolCapabilities {
        catalog_digest: catalog.digest(),
        exposure: match catalog.exposure() {
            qq_core::catalog::Exposure::Full => qq_protocol::ToolExposure::Full,
            qq_core::catalog::Exposure::Progressive => qq_protocol::ToolExposure::Progressive,
        },
        hosts: catalog
            .hosts()
            .iter()
            .map(|host| qq_protocol::ToolHostSummary {
                name: host.name.clone(),
                generation: host.generation,
                tool_count: u32::try_from(host.tool_count).unwrap_or(u32::MAX),
                ready: host.ready,
                message: host.readiness_message.clone(),
            })
            .collect(),
        excluded_tools: u32::try_from(catalog.excluded().len()).unwrap_or(u32::MAX),
        skills: qq_protocol::SkillCapabilities {
            digest: skills.digest(),
            indexed: u32::try_from(skills.len()).unwrap_or(u32::MAX),
            disclosed: u32::try_from(skills.disclosed_count()).unwrap_or(u32::MAX),
            truncated: skills.truncated(),
            entries: skills
                .entries()
                .iter()
                .map(|entry| qq_protocol::SkillSummary {
                    name: entry.name.clone(),
                    kind: entry.kind.into(),
                    source: entry.source.clone(),
                    description: entry.description.clone(),
                    disclosed: entry.disclosed,
                })
                .collect(),
        },
    }
}

impl ServerHandler for RuntimeHandler {
    fn command(&self, request: CommandRequest) -> CommandFuture {
        let runtime = self.durable.clone();
        Box::pin(async move {
            runtime
                .command(request.command_id, request.command)
                .await
                .map_err(map_session_runtime_error)
        })
    }

    fn snapshot(&self, request: SnapshotRequest) -> SnapshotFuture {
        let runtime = self.durable.clone();
        Box::pin(async move {
            runtime
                .snapshot(request)
                .await
                .map_err(map_session_runtime_error)
        })
    }

    fn models(&self, request: ModelCatalogRequest) -> ModelsFuture {
        let factory = self.factory.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || factory.models_for(&request))
                .await
                .map_err(|_| ServerHandlerError::Internal)?
                .map_err(|error| match error.failure_kind() {
                    RunFailureKind::Configuration | RunFailureKind::Policy => {
                        ServerHandlerError::InvalidRequest(error.to_string())
                    }
                    _ => ServerHandlerError::Internal,
                })
        })
    }

    fn profiles(&self, workspace_id: WorkspaceId) -> ProfilesFuture {
        let runtime = self.durable.clone();
        let factory = self.factory.clone();
        Box::pin(async move {
            let workspace = runtime
                .workspace_path(workspace_id)
                .await
                .map_err(map_session_runtime_error)?;
            tokio::task::spawn_blocking(move || factory.profiles_for(&workspace))
                .await
                .map_err(|_| ServerHandlerError::Internal)?
                .map_err(|error| match error.failure_kind() {
                    RunFailureKind::Configuration | RunFailureKind::Policy => {
                        ServerHandlerError::InvalidRequest(error.to_string())
                    }
                    _ => ServerHandlerError::Internal,
                })
        })
    }

    fn delegation(&self, workspace_id: WorkspaceId) -> DelegationFuture {
        let runtime = self.durable.clone();
        let factory = self.factory.clone();
        Box::pin(async move {
            let workspace = runtime
                .workspace_path(workspace_id)
                .await
                .map_err(map_session_runtime_error)?;
            tokio::task::spawn_blocking(move || factory.delegation_for(&workspace))
                .await
                .map_err(|_| ServerHandlerError::Internal)?
                .map_err(|error| match error.failure_kind() {
                    RunFailureKind::Configuration | RunFailureKind::Policy => {
                        ServerHandlerError::InvalidRequest(error.to_string())
                    }
                    _ => ServerHandlerError::Internal,
                })
        })
    }

    fn workspace_tools(&self, workspace_id: WorkspaceId) -> WorkspaceToolsFuture {
        let runtime = self.durable.clone();
        let factory = self.factory.clone();
        Box::pin(async move {
            let workspace = runtime
                .workspace_path(workspace_id)
                .await
                .map_err(map_session_runtime_error)?;
            tokio::task::spawn_blocking(move || {
                let load = LoadRequest::from_process_env(&workspace, None)?;
                let plan = factory.plan_for(&load)?;
                Ok::<_, RuntimeBuildError>(workspace_tool_capabilities(&plan))
            })
            .await
            .map_err(|_| ServerHandlerError::Internal)?
            .map_err(|error| match error.failure_kind() {
                RunFailureKind::Configuration | RunFailureKind::Policy => {
                    ServerHandlerError::InvalidRequest(error.to_string())
                }
                _ => ServerHandlerError::Internal,
            })
        })
    }

    fn subscribe(
        &self,
        request: SubscribeRequest,
    ) -> Result<PublishedEventStream, ServerHandlerError> {
        self.durable
            .subscribe_published(request)
            .map_err(map_session_runtime_error)
    }
}

fn map_session_runtime_error(error: SessionRuntimeError) -> ServerHandlerError {
    match error {
        error @ (SessionRuntimeError::EmptyWorkspace
        | SessionRuntimeError::InvalidWorkspace
        | SessionRuntimeError::EmptyPrompt
        | SessionRuntimeError::PromptTooLarge
        | SessionRuntimeError::InvalidRunLimits
        | SessionRuntimeError::InvalidInput(_)
        | SessionRuntimeError::RunNotSteerable
        | SessionRuntimeError::UnknownProfile(_)
        | SessionRuntimeError::NoCompactionToRollBack
        | SessionRuntimeError::WorkspaceNotFound
        | SessionRuntimeError::SessionNotFound
        | SessionRuntimeError::SessionActive
        | SessionRuntimeError::ParentWorkspaceMismatch
        | SessionRuntimeError::RunNotFound
        | SessionRuntimeError::ToolCallNotFound
        | SessionRuntimeError::ApprovalNotPending
        | SessionRuntimeError::InvalidApprovalGrant
        | SessionRuntimeError::ChildAuthorityEscalation
        | SessionRuntimeError::ChildDepthExceeded
        | SessionRuntimeError::DescendantLimitReached
        | SessionRuntimeError::ContextTooLarge
        | SessionRuntimeError::EventTooLarge
        | SessionRuntimeError::InvalidModelSelection
        | SessionRuntimeError::IdempotencyConflict
        | SessionRuntimeError::CursorStoreMismatch
        | SessionRuntimeError::CursorWorkspaceMismatch
        | SessionRuntimeError::InvalidPageLimit) => {
            ServerHandlerError::InvalidRequest(error.to_string())
        }
        SessionRuntimeError::QueueFull
        | SessionRuntimeError::SteeringQueueFull
        | SessionRuntimeError::WorkspaceLimitReached
        | SessionRuntimeError::SessionLimitReached
        | SessionRuntimeError::CommandLimitReached
        | SessionRuntimeError::Overloaded => ServerHandlerError::Unavailable,
        SessionRuntimeError::InvalidRunLimit
        | SessionRuntimeError::OutputTooLarge
        | SessionRuntimeError::AccountingUnavailable
        | SessionRuntimeError::ShutdownTimedOut
        | SessionRuntimeError::Unavailable
        | SessionRuntimeError::Persistence => ServerHandlerError::Internal,
    }
}

#[derive(Debug, Error)]
pub enum RuntimeHandlerError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Sessions(#[from] SessionRuntimeError),
}

/// Translates the configured roster into the protocol shape, decorating each
/// entry with catalog metadata and its blended price relative to the spawning
/// model (`current`). Relative cost is observational guidance for the model;
/// `None` when either side has no pricing.
fn delegation_roster(
    snapshot: &ConfigSnapshot,
    current: &qq_config::ModelRoute,
) -> qq_protocol::DelegationRoster {
    let metadata = |route: &qq_config::ModelRoute| {
        snapshot
            .providers()
            .get(route.provider())
            .and_then(|provider| provider.models().get(route.model()))
    };
    // A blended per-token price: three input tokens per output token is a
    // rough coding-agent mix and only needs to rank routes, not bill them.
    let blended = |pricing: &qq_config::ModelPricing| -> u128 {
        u128::from(pricing.input_usd_nanos_per_token) * 3
            + u128::from(pricing.output_usd_nanos_per_token)
    };
    let current_price = metadata(current)
        .and_then(qq_config::ModelMetadata::pricing)
        .map(blended)
        .filter(|price| *price > 0);
    let config = snapshot.delegation();
    qq_protocol::DelegationRoster {
        roster: config
            .roster()
            .iter()
            .map(|entry| {
                let metadata = metadata(entry.route());
                qq_protocol::DelegationRosterEntry {
                    route: entry.route().as_str().to_owned(),
                    role: delegation_role(entry.role()),
                    note: entry.note().map(str::to_owned),
                    context_window: metadata.and_then(qq_config::ModelMetadata::context_window),
                    max_output_tokens: metadata
                        .and_then(qq_config::ModelMetadata::max_output_tokens),
                    relative_cost_permille: match (
                        current_price,
                        metadata
                            .and_then(qq_config::ModelMetadata::pricing)
                            .map(blended),
                    ) {
                        (Some(current), Some(entry)) => u32::try_from(entry * 1000 / current).ok(),
                        _ => None,
                    },
                }
            })
            .collect(),
        default_role: delegation_role(config.default_role()),
        max_depth: config.max_depth(),
        write_children: config.write_children(),
    }
}

/// Translates the configured audit section into the runtime policy.
const fn audit_policy(audit: &qq_config::AuditConfig) -> qq_core::AuditPolicy {
    qq_core::AuditPolicy {
        mode: match audit.mode() {
            qq_config::AuditMode::Off => qq_core::AuditMode::Off,
            qq_config::AuditMode::Heuristic => qq_core::AuditMode::Heuristic,
            qq_config::AuditMode::Always => qq_core::AuditMode::Always,
        },
        max_revisions: audit.max_revisions(),
        role: delegation_role(audit.role()),
    }
}

const fn delegation_role(role: qq_config::DelegationRole) -> qq_protocol::DelegationRole {
    match role {
        qq_config::DelegationRole::Fast => qq_protocol::DelegationRole::Fast,
        qq_config::DelegationRole::Balanced => qq_protocol::DelegationRole::Balanced,
        qq_config::DelegationRole::Strong => qq_protocol::DelegationRole::Strong,
    }
}

fn protocol_model_pricing(pricing: qq_config::ModelPricing) -> qq_protocol::ModelPricing {
    qq_protocol::ModelPricing {
        input_usd_nanos_per_token: pricing.input_usd_nanos_per_token,
        output_usd_nanos_per_token: pricing.output_usd_nanos_per_token,
        cache_read_usd_nanos_per_token: pricing.cache_read_usd_nanos_per_token,
        cache_write_usd_nanos_per_token: pricing.cache_write_usd_nanos_per_token,
        context_tier: pricing
            .context_tier
            .map(|tier| qq_protocol::ModelPricingTier {
                above_input_tokens: tier.above_input_tokens,
                input_usd_nanos_per_token: tier.input_usd_nanos_per_token,
                output_usd_nanos_per_token: tier.output_usd_nanos_per_token,
                cache_read_usd_nanos_per_token: tier.cache_read_usd_nanos_per_token,
                cache_write_usd_nanos_per_token: tier.cache_write_usd_nanos_per_token,
            }),
        provenance: pricing.provenance,
    }
}

fn provider_api_name(api: ProviderApi) -> &'static str {
    match api {
        ProviderApi::OpenAiResponses => "openai_responses",
        ProviderApi::OpenAiChatCompletions => "openai_chat_completions",
        ProviderApi::AnthropicMessages => "anthropic_messages",
        ProviderApi::GoogleGenerateContent => "google_generate_content",
        ProviderApi::BedrockConverse => "bedrock_converse",
    }
}

/// Builds the durable provider identity only from validated, non-secret
/// deployment inputs. Custom endpoint provenance, static header values, and
/// URL credential channels are intentionally not hashed: those configurations
/// remain explicitly unknown for cross-run occupancy reuse.
fn provider_request_shape_identity(
    provider_id: &str,
    provider: &ProviderConfig,
    access: &ProviderAccess,
    api: ProviderApi,
) -> Option<ProviderRequestShapeIdentity> {
    if provider.uses_custom_endpoint() {
        return None;
    }
    let mut digest = Sha256::new();
    update_digest(&mut digest, b"qq-provider-request-shape-v1");
    update_digest(&mut digest, provider_id.as_bytes());
    update_digest(&mut digest, provider_api_name(api).as_bytes());
    match access {
        ProviderAccess::Http(access) => {
            // The provider compiler rejects userinfo and fragments. Exact
            // endpoints may otherwise contain a query, so apply the stricter
            // durable-identity rule here before reading any endpoint bytes.
            let endpoint = reqwest::Url::parse(access.endpoint()).ok()?;
            if !endpoint.username().is_empty()
                || endpoint.password().is_some()
                || endpoint.query().is_some()
                || endpoint.fragment().is_some()
                || !access.headers().is_empty()
            {
                return None;
            }
            update_digest(&mut digest, b"http");
            update_digest(&mut digest, endpoint.as_str().as_bytes());
            update_digest(
                &mut digest,
                match access.endpoint_mode() {
                    EndpointMode::Base => b"base",
                    EndpointMode::Exact => b"exact",
                },
            );
            match access.auth() {
                HttpCredential::Configured(ProviderAuth::NoAuth) => {
                    update_digest(&mut digest, b"configured-no-auth");
                }
                HttpCredential::Configured(ProviderAuth::ApiKey(_)) => {
                    update_digest(&mut digest, b"configured-api-key");
                }
                HttpCredential::Configured(ProviderAuth::Bearer(_)) => {
                    update_digest(&mut digest, b"configured-bearer");
                }
                HttpCredential::Configured(ProviderAuth::Header(name, _)) => {
                    update_digest(&mut digest, b"configured-header");
                    update_digest(&mut digest, name.as_bytes());
                }
                HttpCredential::ApiKey { audience, .. } => {
                    update_digest(&mut digest, b"built-in-api-key");
                    update_digest(&mut digest, audience.as_bytes());
                }
                HttpCredential::OpenAiCodex { .. } => {
                    update_digest(&mut digest, b"request-time-codex");
                }
                HttpCredential::XAi { .. } => {
                    update_digest(&mut digest, b"request-time-bearer");
                }
            }
        }
        ProviderAccess::AmazonBedrock { region, auth } => {
            let region = region.as_deref()?;
            update_digest(&mut digest, b"amazon-bedrock-sdk");
            update_digest(&mut digest, region.as_bytes());
            update_digest(&mut digest, bedrock_auth_shape(auth));
        }
        ProviderAccess::AmazonBedrockMantle { region, auth, .. } => {
            let region = region.as_deref()?;
            update_digest(&mut digest, b"amazon-bedrock-mantle");
            update_digest(&mut digest, region.as_bytes());
            update_digest(&mut digest, bedrock_auth_shape(auth));
        }
    }
    Some(ProviderRequestShapeIdentity {
        version: ProviderRequestShapeVersion::new(1)
            .expect("provider request-shape version must be non-zero"),
        digest: qq_protocol::ContentHash::from_bytes(digest.finalize().into()),
    })
}

fn bedrock_auth_shape(auth: &BedrockAuth) -> &'static [u8] {
    match auth {
        BedrockAuth::Aws(AwsAuth::DefaultChain) => b"aws-default-chain",
        BedrockAuth::Aws(AwsAuth::Profile(_)) => b"aws-profile",
        BedrockAuth::ApiKey(_) => b"api-key",
    }
}

fn http_protocol(provider: &str, api: ProviderApi) -> Result<HttpProtocol, RuntimeBuildError> {
    match api {
        ProviderApi::OpenAiResponses => Ok(HttpProtocol::OpenAiResponses),
        ProviderApi::OpenAiChatCompletions => Ok(HttpProtocol::OpenAiChatCompletions),
        ProviderApi::AnthropicMessages => Ok(HttpProtocol::AnthropicMessages),
        ProviderApi::GoogleGenerateContent => Ok(HttpProtocol::GoogleGenerateContent),
        api => Err(RuntimeBuildError::UnsupportedApi {
            provider: provider.to_owned(),
            api,
        }),
    }
}

fn update_digest(digest: &mut Sha256, value: &[u8]) {
    digest.update(u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(value);
}

#[derive(Debug, Error)]
pub enum RuntimeBuildError {
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error(transparent)]
    Provider(#[from] ProviderError),
    #[error(transparent)]
    Runtime(#[from] RuntimeConfigError),
    #[error("configured provider does not exist: {0}")]
    UnknownProvider(String),
    #[error("model {model:?} is not in provider {provider:?}'s authenticated model list")]
    UnknownModel { provider: String, model: String },
    #[error("provider {0:?} is not authenticated; connect it before spawning on it")]
    UnauthenticatedProvider(String),
    #[error("provider {0:?} is missing its connection configuration")]
    IncompleteProvider(String),
    #[error("provider {provider:?} uses an API that is not available yet: {api:?}")]
    UnsupportedApi { provider: String, api: ProviderApi },
    #[error(
        "configured output limit {limit} for {provider}/{model} cannot be represented by its provider codec"
    )]
    UnrepresentableOutputLimit {
        provider: String,
        model: String,
        limit: u32,
    },
    #[error(transparent)]
    Mcp(#[from] qq_mcp::McpConfigError),
    #[error("runtime cache is unavailable")]
    CacheUnavailable,
    #[error("plan cache is full: every cached plan is held by an active run")]
    PlanCacheFull,
    #[error("plan cache has been shut down")]
    PlanCacheShutDown,
    #[error("agent profile {0} is not declared by the workspace configuration")]
    UnknownProfile(AgentProfileId),
    #[error(
        "agent pack {pack} requires protocol version {required}; this build supports {supported}"
    )]
    PackRequiresNewerProtocol {
        pack: String,
        required: u16,
        supported: u16,
    },
    #[error(transparent)]
    Plan(#[from] PlanCompileError),
    #[error(transparent)]
    CatalogClientUnavailable(#[from] crate::catalog::ModelDiscoveryError),
}

impl RuntimeBuildError {
    fn failure_kind(&self) -> RunFailureKind {
        match self {
            Self::Config(ConfigError::PolicyViolation { .. }) => RunFailureKind::Policy,
            Self::Config(_) => RunFailureKind::Configuration,
            Self::Auth(_) => RunFailureKind::Authentication,
            Self::Provider(error) => match error.kind() {
                qq_provider::ProviderErrorKind::Configuration => {
                    RunFailureKind::ProviderConfiguration
                }
                qq_provider::ProviderErrorKind::Authentication => {
                    RunFailureKind::ProviderAuthentication
                }
                qq_provider::ProviderErrorKind::RateLimited => RunFailureKind::ProviderRateLimited,
                qq_provider::ProviderErrorKind::InvalidRequest => {
                    RunFailureKind::ProviderInvalidRequest
                }
                qq_provider::ProviderErrorKind::ContextExceeded => {
                    RunFailureKind::ProviderContextExceeded
                }
                qq_provider::ProviderErrorKind::Unavailable => RunFailureKind::ProviderUnavailable,
                qq_provider::ProviderErrorKind::Transport => RunFailureKind::ProviderTransport,
                qq_provider::ProviderErrorKind::Api => RunFailureKind::ProviderApi,
                qq_provider::ProviderErrorKind::Response => RunFailureKind::ProviderResponse,
                qq_provider::ProviderErrorKind::Protocol => RunFailureKind::ProviderProtocol,
            },
            Self::Mcp(_)
            | Self::UnknownModel { .. }
            | Self::UnknownProfile(_)
            | Self::PackRequiresNewerProtocol { .. } => RunFailureKind::Configuration,
            Self::UnauthenticatedProvider(_) => RunFailureKind::Authentication,
            Self::Runtime(_)
            | Self::UnknownProvider(_)
            | Self::IncompleteProvider(_)
            | Self::UnsupportedApi { .. }
            | Self::UnrepresentableOutputLimit { .. } => RunFailureKind::ProviderConfiguration,
            Self::Plan(_) => RunFailureKind::Configuration,
            Self::CacheUnavailable
            | Self::PlanCacheFull
            | Self::PlanCacheShutDown
            | Self::CatalogClientUnavailable(_) => RunFailureKind::Server,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use super::*;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use futures_util::stream;
    use qq_auth::{CredentialPaths, KeyringBackend, KeyringError};
    use qq_config::{ConfigPaths, ProviderKind, RuntimeOverrides, UsageType};
    use qq_core::Runtime;
    use qq_protocol::{
        CommandId, CommandOutcome, ModelSelection, RunId, RunPromptIdentity, RunStatus,
        SessionCommand, SessionId, WorkspaceId,
    };
    use qq_provider::{ModelRequest, Provider, ProviderEvent, ProviderStream};
    use qq_server::{ServerOptions, ServerPaths, StartOutcome};

    static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct MemoryKeyring(Mutex<BTreeMap<String, Vec<u8>>>);

    impl KeyringBackend for MemoryKeyring {
        fn get(&self, name: &str) -> Result<Vec<u8>, KeyringError> {
            self.0
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .ok_or(KeyringError::Missing)
        }

        fn set(&self, name: &str, secret: &[u8]) -> Result<(), KeyringError> {
            self.0
                .lock()
                .unwrap()
                .insert(name.to_owned(), secret.to_vec());
            Ok(())
        }

        fn remove(&self, name: &str) -> Result<(), KeyringError> {
            self.0
                .lock()
                .unwrap()
                .remove(name)
                .map(|_| ())
                .ok_or(KeyringError::Missing)
        }
    }

    struct RuntimeFixture {
        root: PathBuf,
    }

    impl RuntimeFixture {
        fn new() -> Self {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "qq-runtime-test-{}-{nanos}-{sequence}",
                std::process::id()
            ));
            for directory in ["global", "data", "managed", "work"] {
                fs::create_dir_all(root.join(directory)).unwrap();
            }
            Self { root }
        }

        fn path(&self, relative: impl AsRef<Path>) -> PathBuf {
            self.root.join(relative)
        }

        fn factory(&self) -> RuntimeFactory {
            self.factory_with_credentials(CredentialStore::with_paths(CredentialPaths::new(
                self.path("data"),
            )))
        }

        fn factory_with_credentials(&self, credentials: CredentialStore) -> RuntimeFactory {
            RuntimeFactory::new(
                ConfigLoader::new(ConfigPaths::new(
                    self.path("global"),
                    self.path("data"),
                    self.path("managed"),
                )),
                credentials,
            )
            .unwrap()
        }

        fn request(&self, content: impl Into<String>) -> LoadRequest {
            LoadRequest::new(self.path("work"))
                .with_explicit_content(content)
                .with_overrides(RuntimeOverrides::new().with_max_output_tokens(128))
        }
    }

    impl Drop for RuntimeFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct CapturingProvider {
        requests: Arc<Mutex<Vec<ModelRequest>>>,
    }

    impl Provider for CapturingProvider {
        fn stream(&self, request: ModelRequest) -> ProviderStream {
            self.requests.lock().unwrap().push(request);
            Box::pin(stream::iter([
                Ok(ProviderEvent::OutputTextDelta {
                    text: "done".to_owned(),
                }),
                Ok(ProviderEvent::Completed { usage: None }),
            ]))
        }
    }

    struct FixedRuntimeLoader {
        runtime: Arc<Runtime>,
    }

    impl RuntimeLoader for FixedRuntimeLoader {
        fn load(&self, request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let runtime = Arc::clone(&self.runtime);
            Box::pin(async move {
                LoadedRuntime::compile_blocking(
                    &runtime,
                    ResolvedModel {
                        version: ResolvedModelVersion::new(1).unwrap(),
                        request_shape: None,
                        route: "test/model".to_owned(),
                        provider_model: "test/model".to_owned(),
                        organization: None,
                        credential_profile: None,
                        max_output_tokens: 256,
                        context_window: None,
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
                    },
                    PathBuf::from(request.workspace),
                )
                .map_err(|error| RuntimeLoadError {
                    kind: RunFailureKind::Configuration,
                    message: error.to_string(),
                })
            })
        }
    }

    async fn completed_prompt_identity(
        runtime: &SessionRuntime,
        workspace_id: WorkspaceId,
        session_id: SessionId,
        run_id: RunId,
    ) -> RunPromptIdentity {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let snapshot = runtime
                    .snapshot(SnapshotRequest {
                        workspace_id,
                        focused_session_id: Some(session_id),
                        include_sessions: Vec::new(),
                        session_limit: 8,
                        message_limit: 32,
                    })
                    .await
                    .unwrap();
                if let Some(run) = snapshot
                    .focused
                    .unwrap()
                    .runs
                    .into_iter()
                    .find(|run| run.id == run_id)
                    && run.status == RunStatus::Completed
                {
                    return *run.prompt_identity.unwrap();
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn direct_and_server_commands_resolve_identical_slash_guidance() {
        let fixture = RuntimeFixture::new();
        fs::create_dir_all(fixture.path("work/.qq/skills/review")).unwrap();
        fs::write(
            fixture.path("work/.qq/skills/review/SKILL.md"),
            "Review cancellation and persistence invariants.\n",
        )
        .unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let model_runtime = Arc::new(
            Runtime::new(
                CapturingProvider {
                    requests: Arc::clone(&requests),
                },
                "test/model",
                256,
            )
            .unwrap(),
        );
        let durable = SessionRuntime::open(
            SessionRuntimeOptions::new(fixture.path("sessions.sqlite3")),
            Arc::new(FixedRuntimeLoader {
                runtime: model_runtime,
            }),
        )
        .await
        .unwrap();
        let handler = Arc::new(RuntimeHandler {
            durable,
            factory: fixture.factory(),
        });
        let resolved = handler
            .sessions()
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::ResolveWorkspace {
                    path: fixture.path("work").display().to_string(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::WorkspaceResolved { workspace_id } = resolved.outcome else {
            panic!("unexpected receipt")
        };
        let mut sessions = Vec::new();
        for _ in 0..2 {
            let created = handler
                .sessions()
                .command(
                    CommandId::generate().unwrap(),
                    SessionCommand::CreateSession {
                        workspace_id,
                        parent_id: None,
                        model: ModelSelection {
                            model: Some("test/model".to_owned()),
                            max_output_tokens: Some(256),
                            organization: None,
                        },
                        approval_mode: qq_protocol::ApprovalMode::Ask,
                        profile: qq_protocol::AgentProfileId::default(),
                        correlation: qq_protocol::Correlation::default(),
                    },
                )
                .await
                .unwrap();
            let CommandOutcome::SessionCreated { session_id } = created.outcome else {
                panic!("unexpected receipt")
            };
            sessions.push(session_id);
        }

        let direct = handler
            .sessions()
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::SubmitPrompt {
                    session_id: sessions[0],
                    input: vec![qq_protocol::InputPart::text(
                        "/review focus on cancellation".to_owned(),
                    )],
                    limits: qq_protocol::RunLimits::default(),
                    correlation: qq_protocol::Correlation::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::PromptQueued {
            run_id: direct_run, ..
        } = direct.outcome
        else {
            panic!("unexpected receipt")
        };
        let direct_identity =
            completed_prompt_identity(handler.sessions(), workspace_id, sessions[0], direct_run)
                .await;

        let server = match qq_server::start(
            handler.clone(),
            ServerOptions::new(ServerPaths::new(fixture.path("server"))),
        )
        .await
        .unwrap()
        {
            StartOutcome::Started(server) => server,
            StartOutcome::Existing(_) => panic!("test unexpectedly found a running server"),
        };
        let server_command = CommandRequest {
            command_id: CommandId::generate().unwrap(),
            command: SessionCommand::SubmitPrompt {
                session_id: sessions[1],
                input: vec![qq_protocol::InputPart::text(
                    "/review focus on cancellation".to_owned(),
                )],
                limits: qq_protocol::RunLimits::default(),
                correlation: qq_protocol::Correlation::default(),
            },
        };
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .post(server.connection().endpoint("/v1/sessions/prompts"))
            .bearer_auth(server.connection().expose_bearer_token())
            .json(&server_command)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let receipt = response
            .json::<qq_protocol::CommandReceipt>()
            .await
            .unwrap();
        let CommandOutcome::PromptQueued {
            run_id: server_run, ..
        } = receipt.outcome
        else {
            panic!("unexpected receipt")
        };
        let server_identity =
            completed_prompt_identity(handler.sessions(), workspace_id, sessions[1], server_run)
                .await;

        assert_eq!(direct_identity, server_identity);
        {
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].system(), requests[1].system());
            assert!(
                requests[0]
                    .system()
                    .unwrap()
                    .contains("Review cancellation and persistence invariants.")
            );
        }
        server.shutdown().await.unwrap();
        handler.shutdown().await.unwrap();
    }

    struct CollectingSink {
        seen: Vec<qq_protocol::SessionEventEnvelope>,
        stop_at: Option<u64>,
        stall_first: Option<tokio::sync::oneshot::Receiver<()>>,
        disconnects: usize,
    }

    impl qq_client::observer::EventSink for CollectingSink {
        fn deliver(
            &mut self,
            event: &qq_protocol::SessionEventEnvelope,
        ) -> qq_client::observer::SinkFuture<'_> {
            let event = event.clone();
            Box::pin(async move {
                if let Some(gate) = self.stall_first.take() {
                    // Hold the first delivery until the test says the store
                    // has kept committing without us.
                    let _ = gate.await;
                }
                let sequence = event.cursor.sequence;
                self.seen.push(event);
                if self.stop_at == Some(sequence) {
                    qq_client::observer::ObserverStep::Stop
                } else {
                    qq_client::observer::ObserverStep::Continue
                }
            })
        }

        fn disconnected(
            &mut self,
            _error: &qq_client::ClientError,
            _resume_from: &qq_protocol::EventCursor,
        ) -> qq_client::observer::ObserverStep {
            self.disconnects += 1;
            qq_client::observer::ObserverStep::Continue
        }
    }

    /// An observer that falls behind by more than a replay page, stops, and
    /// restarts from its cursor receives exactly the committed sequence a
    /// live subscriber received; a stalled observer never delays commits.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn observers_fall_behind_restart_and_converge_without_delaying_commits() {
        let fixture = RuntimeFixture::new();
        fs::create_dir_all(fixture.path("work")).unwrap();
        let model_runtime = Arc::new(
            Runtime::new(
                CapturingProvider {
                    requests: Arc::new(Mutex::new(Vec::new())),
                },
                "test/model",
                256,
            )
            .unwrap(),
        );
        let durable = SessionRuntime::open(
            SessionRuntimeOptions::new(fixture.path("sessions.sqlite3")),
            Arc::new(FixedRuntimeLoader {
                runtime: model_runtime,
            }),
        )
        .await
        .unwrap();
        let handler = Arc::new(RuntimeHandler {
            durable,
            factory: fixture.factory(),
        });
        let server = match qq_server::start(
            handler.clone(),
            ServerOptions::new(ServerPaths::new(fixture.path("server"))),
        )
        .await
        .unwrap()
        {
            StartOutcome::Started(server) => server,
            StartOutcome::Existing(_) => panic!("test unexpectedly found a running server"),
        };
        let client = qq_client::SessionClient::new(server.connection().clone()).unwrap();
        let (workspace_id, origin) = client
            .resolve_workspace(&fixture.path("work"))
            .await
            .unwrap();
        let created = client
            .command(
                CommandId::generate().unwrap(),
                SessionCommand::CreateSession {
                    workspace_id,
                    parent_id: None,
                    model: ModelSelection {
                        model: Some("test/model".to_owned()),
                        max_output_tokens: Some(256),
                        organization: None,
                    },
                    approval_mode: qq_protocol::ApprovalMode::Ask,
                    profile: qq_protocol::AgentProfileId::default(),
                    correlation: qq_protocol::Correlation::default(),
                },
            )
            .await
            .unwrap();
        let CommandOutcome::SessionCreated { session_id } = created.outcome else {
            panic!("unexpected receipt")
        };

        // A live subscriber records the authoritative sequence as it happens.
        let live_client = client.clone();
        let live_origin = origin;
        let live = tokio::spawn(async move {
            let mut stream = live_client.events(workspace_id, live_origin).await.unwrap();
            let mut seen = Vec::new();
            while let Some(Ok(event)) = futures_util::StreamExt::next(&mut stream).await {
                let done = matches!(&event.event, qq_protocol::SessionEvent::RunFinished { .. })
                    && seen.len() > 300;
                seen.push(event);
                if done {
                    break;
                }
            }
            seen
        });

        // A stalled observer holds its first delivery while the store keeps
        // committing: commits must not wait for it.
        let (release, gate) = tokio::sync::oneshot::channel();
        let mut stalled = CollectingSink {
            seen: Vec::new(),
            stop_at: None,
            stall_first: Some(gate),
            disconnects: 0,
        };
        let stalled_client = client.clone();
        let stalled_origin = origin;
        let stalled_task = tokio::spawn(async move {
            qq_client::observer::run(&stalled_client, workspace_id, stalled_origin, &mut stalled)
                .await
        });

        // Generate well over one replay page (128) of events. The prompt
        // queue is bounded (16 pending), so a full queue is backpressure
        // (503) that the producer waits out; it is never dropped work.
        let mut acks = Vec::new();
        for i in 0..40 {
            loop {
                let started = std::time::Instant::now();
                match client
                    .submit(
                        session_id,
                        vec![qq_protocol::InputPart::text(format!("prompt {i}"))],
                        qq_protocol::RunLimits::default(),
                        qq_protocol::Correlation::default(),
                    )
                    .await
                {
                    Ok(receipt) => {
                        acks.push(started.elapsed());
                        assert!(matches!(
                            receipt.outcome,
                            CommandOutcome::PromptQueued { .. }
                        ));
                        break;
                    }
                    Err(qq_client::ClientError::ServerMessage { status: 503, .. }) => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    Err(error) => panic!("submit failed: {error}"),
                }
            }
        }
        let live_events = tokio::time::timeout(Duration::from_secs(30), live)
            .await
            .unwrap()
            .unwrap();
        assert!(
            live_events.len() > 128,
            "{} events is not more than one page",
            live_events.len()
        );
        // Acknowledgements stayed fast while an observer was stalled.
        let mut sorted = acks.clone();
        sorted.sort();
        assert!(
            sorted[sorted.len() / 2] < Duration::from_millis(250),
            "median ack {:?} with a stalled observer",
            sorted[sorted.len() / 2]
        );
        stalled_task.abort();
        drop(release);

        // A fresh observer replays from the origin, stops partway (as if it
        // crashed), and a second observer resumes from the acknowledged
        // cursor. Together they reproduce the live sequence exactly.
        let midpoint = live_events[live_events.len() / 2].cursor.sequence;
        let mut first = CollectingSink {
            seen: Vec::new(),
            stop_at: Some(midpoint),
            stall_first: None,
            disconnects: 0,
        };
        let (exit, resume_from) = tokio::time::timeout(
            Duration::from_secs(30),
            qq_client::observer::run(&client, workspace_id, origin, &mut first),
        )
        .await
        .unwrap();
        assert_eq!(exit, qq_client::observer::ObserverExit::Stopped);
        // The event being delivered when the sink stopped is not acknowledged.
        assert_eq!(resume_from.sequence, midpoint - 1);
        first.seen.pop();
        let last_live = live_events.last().unwrap().cursor.sequence;
        let mut second = CollectingSink {
            seen: Vec::new(),
            stop_at: Some(last_live),
            stall_first: None,
            disconnects: 0,
        };
        let (exit, _) = tokio::time::timeout(
            Duration::from_secs(30),
            qq_client::observer::run(&client, workspace_id, resume_from, &mut second),
        )
        .await
        .unwrap();
        assert_eq!(exit, qq_client::observer::ObserverExit::Stopped);
        let mut replayed = first.seen;
        replayed.extend(second.seen);
        assert_eq!(replayed.len(), live_events.len());
        for (replayed, live) in replayed.iter().zip(&live_events) {
            assert_eq!(
                serde_json::to_vec(replayed).unwrap(),
                serde_json::to_vec(live).unwrap(),
                "replay must be byte-identical to live delivery"
            );
        }
        assert_eq!(first.disconnects + second.disconnects, 0);

        // A cursor from another store is rejected, not silently resumed.
        let foreign = qq_protocol::EventCursor {
            store_id: qq_protocol::StoreId::generate().unwrap(),
            workspace_id,
            sequence: 1,
        };
        let mut sink = CollectingSink {
            seen: Vec::new(),
            stop_at: None,
            stall_first: None,
            disconnects: 0,
        };
        let (exit, _) = tokio::time::timeout(
            Duration::from_secs(10),
            qq_client::observer::run(&client, workspace_id, foreign, &mut sink),
        )
        .await
        .unwrap();
        assert_eq!(exit, qq_client::observer::ObserverExit::CursorRejected);
        assert!(sink.seen.is_empty());

        server.shutdown().await.unwrap();
        handler.shutdown().await.unwrap();
    }

    /// The interactive client delivers the workspace-scoped capability
    /// document (profiles included) after bootstrap and again on request, so
    /// a TUI can list profiles without a restart after a pack is added.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tui_client_delivers_workspace_capabilities_and_refreshes_them() {
        use qq_client::{ClientPort as _, ClientRequest, ClientUpdate};

        let fixture = RuntimeFixture::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(fixture.path("data"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::create_dir_all(fixture.path("work/.qq/skills/qq-verify")).unwrap();
        fs::write(
            fixture.path("work/.qq/skills/qq-verify/SKILL.md"),
            "---\ndescription: Run the gates.\n---\n# Verify\n",
        )
        .unwrap();
        fs::write(
            fixture.path("work/.qq/config.ron"),
            r#"(version: 1, model: "custom/test-model", providers: { "custom": Custom(connection: (base_url: "http://127.0.0.1:1/v1", api: OpenAiResponses, auth: NoAuth), models: { "test-model": (name: "Test model") }) })"#,
        )
        .unwrap();
        let model_runtime = Arc::new(
            Runtime::new(
                CapturingProvider {
                    requests: Arc::new(Mutex::new(Vec::new())),
                },
                "test/model",
                256,
            )
            .unwrap(),
        );
        let durable = SessionRuntime::open(
            SessionRuntimeOptions::new(fixture.path("sessions.sqlite3")),
            Arc::new(FixedRuntimeLoader {
                runtime: model_runtime,
            }),
        )
        .await
        .unwrap();
        let handler = Arc::new(RuntimeHandler {
            durable,
            factory: fixture.factory(),
        });
        let server = match qq_server::start(
            handler.clone(),
            ServerOptions::new(ServerPaths::new(fixture.path("server"))),
        )
        .await
        .unwrap()
        {
            StartOutcome::Started(server) => server,
            StartOutcome::Existing(_) => panic!("test unexpectedly found a running server"),
        };
        // Project configuration is sensitive: capabilities for an untrusted
        // workspace fail as configuration, and the client leaves the document
        // unadvertised. Trust it up front, as `qq trust` would.
        let request = LoadRequest::new(fixture.path("work"));
        handler
            .factory
            .inner
            .config
            .grant_pending_trust(&request)
            .unwrap();
        let mut tui = qq_client::TuiClient::start(
            server.connection().clone(),
            fixture.path("work"),
            ModelSelection {
                model: Some("custom/test-model".to_owned()),
                max_output_tokens: Some(256),
                organization: None,
            },
            None,
            false,
            || std::future::ready(None),
        )
        .unwrap();
        async fn next_capabilities(
            tui: &mut qq_client::TuiClient,
        ) -> Arc<qq_protocol::ServerCapabilities> {
            loop {
                match tokio::time::timeout(Duration::from_secs(10), tui.recv())
                    .await
                    .expect("client update")
                    .expect("client alive")
                {
                    ClientUpdate::Capabilities(capabilities) => return capabilities,
                    ClientUpdate::SnapshotFailed(failure) => panic!("{}", failure.message()),
                    _ => {}
                }
            }
        }
        let initial = next_capabilities(&mut tui).await;
        let profiles = initial
            .profiles
            .as_deref()
            .expect("workspace-scoped document");
        assert_eq!(profiles.len(), 1);
        assert!(profiles[0].id.is_default());
        // The skill index is listed entry by entry so a client can complete
        // and describe the workspace's guidance.
        let skills = &initial.workspace_tools.as_ref().unwrap().skills;
        assert_eq!(skills.indexed, 1);
        assert_eq!(skills.entries.len(), 1);
        assert_eq!(skills.entries[0].name, "qq-verify");
        assert_eq!(skills.entries[0].kind, qq_protocol::GuidanceKind::Skill);
        assert_eq!(skills.entries[0].source, ".qq/skills/qq-verify/SKILL.md");
        assert_eq!(skills.entries[0].description, "Run the gates.");
        assert!(skills.entries[0].disclosed);
        assert!(initial.steering.boundary);

        // A pack dropped into the trusted workspace appears on refresh.
        fs::create_dir_all(fixture.path("work/.qq/packs/kit")).unwrap();
        fs::write(
            fixture.path("work/.qq/packs/kit/pack.ron"),
            r#"(schema: 1, id: "kit", version: "1.0.0", profiles: { "reviewer": (approval_mode: read_only) })"#,
        )
        .unwrap();
        handler
            .factory
            .inner
            .config
            .grant_pending_trust(&request)
            .unwrap();
        tui.try_send(ClientRequest::Capabilities).unwrap();
        let refreshed = next_capabilities(&mut tui).await;
        let profiles = refreshed.profiles.as_deref().unwrap();
        let reviewer = profiles
            .iter()
            .find(|profile| profile.id.as_str() == "reviewer")
            .expect("pack profile advertised");
        assert_eq!(reviewer.approval_mode, qq_protocol::ApprovalMode::ReadOnly);
        assert_eq!(reviewer.pack.as_ref().unwrap().id, "kit");

        drop(tui);
        server.shutdown().await.unwrap();
        handler.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn runtime_factory_can_be_dropped_in_async_context() {
        let fixture = RuntimeFixture::new();

        drop(fixture.factory());
    }

    #[test]
    fn protocol_pricing_adapter_preserves_every_field() {
        let pricing = qq_config::ModelPricing {
            input_usd_nanos_per_token: 1,
            output_usd_nanos_per_token: 2,
            cache_read_usd_nanos_per_token: Some(3),
            cache_write_usd_nanos_per_token: Some(4),
            context_tier: Some(qq_config::ModelPricingTier {
                above_input_tokens: 5,
                input_usd_nanos_per_token: 6,
                output_usd_nanos_per_token: 7,
                cache_read_usd_nanos_per_token: Some(8),
                cache_write_usd_nanos_per_token: Some(9),
            }),
            provenance: "test catalog".to_owned(),
        };

        assert_eq!(
            protocol_model_pricing(pricing),
            qq_protocol::ModelPricing {
                input_usd_nanos_per_token: 1,
                output_usd_nanos_per_token: 2,
                cache_read_usd_nanos_per_token: Some(3),
                cache_write_usd_nanos_per_token: Some(4),
                context_tier: Some(qq_protocol::ModelPricingTier {
                    above_input_tokens: 5,
                    input_usd_nanos_per_token: 6,
                    output_usd_nanos_per_token: 7,
                    cache_read_usd_nanos_per_token: Some(8),
                    cache_write_usd_nanos_per_token: Some(9),
                }),
                provenance: "test catalog".to_owned(),
            }
        );
    }

    #[test]
    fn resolved_model_caps_output_to_known_metadata_and_preserves_unknown_limits() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let resolve = |model_metadata: &str, configured_limit| {
            let request = LoadRequest::new(fixture.path("work"))
                .with_explicit_content(format!(
                    r#"(
                        version: 1,
                        model: "custom/test-model",
                        providers: {{
                            "custom": Custom(
                                connection: (
                                    base_url: "http://127.0.0.1:1/v1",
                                    api: OpenAiResponses,
                                    auth: NoAuth,
                                ),
                                models: {{"test-model": ({model_metadata})}},
                            ),
                        }},
                    )"#
                ))
                .with_overrides(RuntimeOverrides::new().with_max_output_tokens(configured_limit));
            let snapshot = factory.load(&request).unwrap();
            factory.resolved_model_for_snapshot(&snapshot).unwrap()
        };

        assert_eq!(
            resolve("name: \"Capped\", max_output_tokens: 64", 128).max_output_tokens,
            64
        );
        assert_eq!(
            resolve("name: \"Equal\", max_output_tokens: 128", 128).max_output_tokens,
            128
        );
        assert_eq!(resolve("name: \"Unknown\"", 128).max_output_tokens, 128);
    }

    #[test]
    fn resolved_model_is_secret_free_and_carries_effective_metadata() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let request = LoadRequest::new(fixture.path("work"))
            .with_explicit_content(
                r#"(
                    version: 1,
                    organization: "org-a",
                    model: "custom/test-model",
                    providers: {
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: ApiKey(Value("sk-super-secret")),
                            ),
                            models: {
                                "test-model": (
                                    name: "Test model",
                                    context_window: 32768,
                                    max_output_tokens: 64,
                                    pricing: (
                                        input_usd_nanos_per_token: 1,
                                        output_usd_nanos_per_token: 2,
                                        cache_read_usd_nanos_per_token: 1,
                                        provenance: "catalog-a",
                                    ),
                                ),
                            },
                        ),
                    },
                )"#,
            )
            .with_overrides(RuntimeOverrides::new().with_max_output_tokens(128));
        let snapshot = factory.load(&request).unwrap();

        let resolved = factory.resolved_model_for_snapshot(&snapshot).unwrap();

        assert_eq!(resolved.version.get(), 2);
        assert_eq!(resolved.request_shape, None);
        assert_eq!(resolved.route, "custom/test-model");
        assert_eq!(resolved.provider_model, "test-model");
        assert_eq!(resolved.organization.as_deref(), Some("org-a"));
        assert_eq!(resolved.credential_profile, None);
        assert_eq!(resolved.max_output_tokens, 64);
        assert_eq!(resolved.context_window, Some(32_768));
        assert_eq!(
            resolved
                .pricing
                .as_ref()
                .map(|pricing| pricing.provenance.as_str()),
            Some("catalog-a")
        );
        assert_eq!(resolved.output_token_control, CapabilitySupport::Native);
        assert_eq!(
            resolved.generation.reasoning_effort,
            CapabilitySupport::Unsupported
        );
        assert_eq!(
            resolved.prompt_cache.control,
            CapabilitySupport::Unsupported
        );
        assert!(resolved.prompt_cache.cache_read_usage);
        assert!(!resolved.prompt_cache.cache_write_usage);
        let encoded = serde_json::to_string(&resolved).unwrap();
        assert!(!encoded.contains("sk-super-secret"), "{encoded}");
        assert!(!encoded.contains("api_key"), "{encoded}");
    }

    #[test]
    fn request_shape_identity_stays_unknown_for_secret_bearing_deployment_channels() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let resolve = |connection: &str| {
            let request = LoadRequest::new(fixture.path("work")).with_explicit_content(format!(
                r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {{
                        "custom": Custom(
                            connection: ({connection}),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
            ));
            let snapshot = factory.load(&request).unwrap();
            factory.resolved_model_for_snapshot(&snapshot).unwrap()
        };

        for (connection, forbidden) in [
            (
                r#"base_url: "https://user:credential@example.test/v1", api: OpenAiResponses, auth: NoAuth"#,
                "credential",
            ),
            (
                r#"base_url: "https://example.test/v1?api_key=query-secret", api: OpenAiResponses, auth: NoAuth"#,
                "query-secret",
            ),
            (
                r#"base_url: "https://example.test/v1", api: OpenAiResponses, auth: NoAuth, headers: {"x-secret": "header-secret"}"#,
                "header-secret",
            ),
            (
                r#"base_url: "https://example.test/v1/path-secret", api: OpenAiResponses, auth: NoAuth"#,
                "path-secret",
            ),
        ] {
            let resolved = resolve(connection);
            assert_eq!(resolved.version.get(), 2);
            assert_eq!(resolved.request_shape, None);
            let encoded = serde_json::to_string(&resolved).unwrap();
            assert!(!encoded.contains(forbidden), "{encoded}");
            let forbidden_digest = format!("{:x}", Sha256::digest(forbidden.as_bytes()));
            assert!(!encoded.contains(&forbidden_digest), "{encoded}");
            assert!(!encoded.contains("request_shape"), "{encoded}");
        }
    }

    #[test]
    fn request_shape_identity_is_canonical_and_tracks_every_safe_wire_input() {
        let trusted_http = ProviderConfig::new(
            ProviderKind::OpenAi,
            None,
            UsageType::Metered,
            BTreeMap::new(),
        );
        let trusted_bedrock = ProviderConfig::new(
            ProviderKind::AmazonBedrock,
            None,
            UsageType::Metered,
            BTreeMap::new(),
        );
        let http = |endpoint: &str, mode: EndpointMode, api: ProviderApi, auth: ProviderAuth| {
            ProviderAccess::Http(HttpAccess::new(
                endpoint,
                mode,
                api,
                HttpCredential::Configured(auth),
                BTreeMap::new(),
            ))
        };
        let api_key = |value: &str| {
            ProviderAuth::ApiKey(qq_config::SecretRef::Value(qq_config::SecretLiteral::new(
                value,
            )))
        };
        let base = http(
            "https://example.test/v1",
            EndpointMode::Base,
            ProviderApi::OpenAiResponses,
            api_key("first-secret"),
        );
        let identity = provider_request_shape_identity(
            "openai",
            &trusted_http,
            &base,
            ProviderApi::OpenAiResponses,
        )
        .unwrap();
        let normalized = http(
            "HTTPS://EXAMPLE.TEST:443/v1",
            EndpointMode::Base,
            ProviderApi::OpenAiResponses,
            api_key("rotated-secret"),
        );
        assert_eq!(
            provider_request_shape_identity(
                "openai",
                &trusted_http,
                &normalized,
                ProviderApi::OpenAiResponses,
            ),
            Some(identity),
            "URL spelling and credential rotation do not change wire shape"
        );

        let variants = [
            http(
                "https://other.example.test/v1",
                EndpointMode::Base,
                ProviderApi::OpenAiResponses,
                api_key("first-secret"),
            ),
            http(
                "https://example.test/v1",
                EndpointMode::Exact,
                ProviderApi::OpenAiResponses,
                api_key("first-secret"),
            ),
            http(
                "https://example.test/v1",
                EndpointMode::Base,
                ProviderApi::OpenAiChatCompletions,
                api_key("first-secret"),
            ),
            http(
                "https://example.test/v1",
                EndpointMode::Base,
                ProviderApi::OpenAiResponses,
                ProviderAuth::Bearer(qq_config::SecretRef::Value(qq_config::SecretLiteral::new(
                    "first-secret",
                ))),
            ),
        ];
        for variant in variants {
            let ProviderAccess::Http(access) = &variant else {
                unreachable!()
            };
            assert_ne!(
                provider_request_shape_identity("openai", &trusted_http, &variant, access.api()),
                Some(identity)
            );
        }

        let bedrock = ProviderAccess::AmazonBedrock {
            region: Some("us-east-1".to_owned()),
            auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
        };
        let other_region = ProviderAccess::AmazonBedrock {
            region: Some("us-west-2".to_owned()),
            auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
        };
        assert_ne!(
            provider_request_shape_identity(
                "bedrock",
                &trusted_bedrock,
                &bedrock,
                ProviderApi::BedrockConverse,
            ),
            provider_request_shape_identity(
                "bedrock",
                &trusted_bedrock,
                &other_region,
                ProviderApi::BedrockConverse,
            )
        );
        assert_eq!(
            provider_request_shape_identity(
                "bedrock",
                &trusted_bedrock,
                &ProviderAccess::AmazonBedrock {
                    region: None,
                    auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
                },
                ProviderApi::BedrockConverse,
            ),
            None,
            "a dynamic AWS region chain cannot produce a durable exact identity"
        );

        // Built-in deployments also stay unknown when the endpoint or static
        // headers could carry a secret. Custom/LiteLLM return before these
        // guards run, so exercise them through a trusted provider kind.
        let guarded = [
            http(
                "https://user:credential@example.test/v1",
                EndpointMode::Base,
                ProviderApi::OpenAiResponses,
                ProviderAuth::NoAuth,
            ),
            http(
                "https://example.test/v1?api_key=query-secret",
                EndpointMode::Exact,
                ProviderApi::OpenAiResponses,
                ProviderAuth::NoAuth,
            ),
            http(
                "https://example.test/v1#fragment",
                EndpointMode::Base,
                ProviderApi::OpenAiResponses,
                ProviderAuth::NoAuth,
            ),
            ProviderAccess::Http(HttpAccess::new(
                "https://example.test/v1",
                EndpointMode::Base,
                ProviderApi::OpenAiResponses,
                HttpCredential::Configured(ProviderAuth::NoAuth),
                serde_json::from_value(serde_json::json!({"x-secret": "header-secret"})).unwrap(),
            )),
        ];
        for access in guarded {
            assert_eq!(
                provider_request_shape_identity(
                    "openai",
                    &trusted_http,
                    &access,
                    ProviderApi::OpenAiResponses,
                ),
                None,
                "{access:?}"
            );
        }
    }

    #[test]
    fn resolved_model_reports_codex_controls_and_named_auth_profiles() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let codex = factory
            .load(&fixture.request(
                r#"(
                    version: 1,
                    model: "openai-codex/gpt-test",
                    providers: {
                        "openai-codex": OpenAiCodex(
                            profile: "work",
                            models: {"gpt-test": (name: "Codex test")},
                        ),
                    },
                )"#,
            ))
            .unwrap();
        let codex = factory.resolved_model_for_snapshot(&codex).unwrap();
        assert_eq!(codex.credential_profile.as_deref(), Some("work"));
        assert_eq!(codex.output_token_control, CapabilitySupport::Unsupported);
        assert_eq!(
            codex.generation.reasoning_effort,
            CapabilitySupport::Unsupported
        );
        assert_eq!(codex.prompt_cache.control, CapabilitySupport::Unsupported);
        assert!(codex.prompt_cache.cache_read_usage);
        assert!(!codex.prompt_cache.cache_write_usage);

        let bedrock = factory
            .load(&fixture.request(
                r#"(
                    version: 1,
                    model: "bedrock/test-model",
                    providers: {
                        "bedrock": AmazonBedrock(
                            region: "us-east-1",
                            auth: Aws(Profile("aws-work")),
                            models: {"test-model": (name: "Bedrock test")},
                        ),
                    },
                )"#,
            ))
            .unwrap();
        let bedrock = factory.resolved_model_for_snapshot(&bedrock).unwrap();
        assert_eq!(bedrock.credential_profile.as_deref(), Some("aws-work"));
        assert!(bedrock.prompt_cache.cache_read_usage);
        assert!(bedrock.prompt_cache.cache_write_usage);
    }

    #[test]
    fn delegation_roster_carries_catalog_metadata_and_relative_cost() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let snapshot = factory
            .load(&fixture.request(
                r#"(
                    version: 1,
                    model: "custom/main",
                    delegation: (
                        roster: [
                            (route: "custom/fast", role: fast, note: "lookups"),
                            (route: "custom/main", role: balanced),
                            (route: "custom/unpriced", role: strong),
                        ],
                        default_role: fast,
                        max_depth: 2,
                    ),
                    providers: {
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: NoAuth,
                            ),
                            models: {
                                "main": (
                                    name: "Main",
                                    context_window: 200000,
                                    max_output_tokens: 8192,
                                    pricing: (
                                        input_usd_nanos_per_token: 100,
                                        output_usd_nanos_per_token: 200,
                                        provenance: "fixture",
                                    ),
                                ),
                                "fast": (
                                    name: "Fast",
                                    context_window: 400000,
                                    pricing: (
                                        input_usd_nanos_per_token: 10,
                                        output_usd_nanos_per_token: 20,
                                        provenance: "fixture",
                                    ),
                                ),
                                "unpriced": (name: "Unpriced"),
                            },
                        ),
                    },
                )"#,
            ))
            .unwrap();
        let roster = delegation_roster(&snapshot, snapshot.model());
        assert_eq!(roster.default_role, qq_protocol::DelegationRole::Fast);
        assert_eq!(roster.max_depth, 2);
        assert!(!roster.write_children);
        assert_eq!(roster.roster.len(), 3);
        let fast = &roster.roster[0];
        assert_eq!(fast.route, "custom/fast");
        assert_eq!(fast.role, qq_protocol::DelegationRole::Fast);
        assert_eq!(fast.note.as_deref(), Some("lookups"));
        assert_eq!(fast.context_window, Some(400_000));
        assert_eq!(fast.max_output_tokens, None);
        // Blended 3:1 price: fast (10*3+20)=50 vs main (100*3+200)=500.
        assert_eq!(fast.relative_cost_permille, Some(100));
        let main = &roster.roster[1];
        assert_eq!(main.relative_cost_permille, Some(1000));
        assert_eq!(main.max_output_tokens, Some(8_192));
        let unpriced = &roster.roster[2];
        assert_eq!(unpriced.relative_cost_permille, None);
        assert_eq!(unpriced.context_window, None);

        // The compiled plan records the roster in its descriptor and the
        // spawn_agent schema offers roles.
        let plan = factory
            .plan_for(&fixture.request(
                r#"(
                    version: 1,
                    model: "custom/main",
                    delegation: (roster: [(route: "custom/fast", role: fast)], default_role: fast),
                    providers: {
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: NoAuth,
                            ),
                            models: {"main": (name: "Main"), "fast": (name: "Fast")},
                        ),
                    },
                )"#,
            ))
            .unwrap();
        assert_eq!(plan.descriptor().version, 5);
        assert_eq!(plan.descriptor().delegation.roster.len(), 1);
        assert_eq!(plan.descriptor().delegation.roster[0].route, "custom/fast");
        assert_eq!(
            plan.descriptor().delegation.default_role,
            qq_protocol::DelegationRole::Fast
        );
    }

    #[test]
    fn resolved_model_rejects_output_limits_the_codec_cannot_represent() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let request = LoadRequest::new(fixture.path("work"))
            .with_explicit_content(
                r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: GoogleGenerateContent,
                                auth: NoAuth,
                            ),
                            models: {"test-model": (name: "Google test")},
                        ),
                    },
                )"#,
            )
            .with_overrides(RuntimeOverrides::new().with_max_output_tokens(u32::MAX));
        let snapshot = factory.load(&request).unwrap();

        assert!(matches!(
            factory.resolved_model_for_snapshot(&snapshot),
            Err(RuntimeBuildError::UnrepresentableOutputLimit {
                provider,
                model,
                limit: u32::MAX,
            }) if provider == "custom" && model == "test-model"
        ));
    }

    #[tokio::test]
    async fn resolves_configured_worker_model_and_falls_back_to_parent_selection() {
        let fixture = RuntimeFixture::new();
        let workspace = fs::canonicalize(fixture.path("work")).unwrap();
        fs::write(
            fixture.path("global/config.ron"),
            r#"(
                version: 1,
                model: "custom/default",
                worker_model: "custom/worker",
                max_output_tokens: 321,
                organization: "configured-org",
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                    ),
                },
            )"#,
        )
        .unwrap();
        let factory = fixture.factory();
        let parent = qq_protocol::ModelSelection {
            model: Some("custom/persisted".to_owned()),
            max_output_tokens: Some(123),
            organization: Some("parent-org".to_owned()),
        };

        let resolved = RuntimeLoader::resolve_worker_model(
            &factory,
            workspace.display().to_string(),
            parent.clone(),
        )
        .await
        .unwrap();
        assert_eq!(resolved.model.as_deref(), Some("custom/worker"));
        assert_eq!(resolved.max_output_tokens, Some(123));
        assert_eq!(resolved.organization.as_deref(), Some("parent-org"));

        fs::write(
            fixture.path("global/config.ron"),
            r#"(
                version: 1,
                model: "custom/default",
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                    ),
                },
            )"#,
        )
        .unwrap();
        let fallback = RuntimeLoader::resolve_worker_model(
            &factory,
            workspace.display().to_string(),
            parent.clone(),
        )
        .await
        .unwrap();
        assert_eq!(fallback, parent);
    }

    async fn validate_route(
        factory: &RuntimeFactory,
        workspace: &Path,
        route: &str,
    ) -> Result<(), RuntimeLoadError> {
        RuntimeLoader::validate_spawn_model(
            factory,
            workspace.display().to_string(),
            qq_protocol::ModelSelection {
                model: Some(route.to_owned()),
                max_output_tokens: Some(128),
                organization: None,
            },
        )
        .await
    }

    #[tokio::test]
    async fn spawn_validation_names_each_failed_check() {
        let fixture = RuntimeFixture::new();
        let workspace = fs::canonicalize(fixture.path("work")).unwrap();
        fs::write(
            fixture.path("global/config.ron"),
            r#"(
                version: 1,
                model: "custom/known",
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {"known": (name: "Known model")},
                    ),
                    "bare": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                    ),
                },
            )"#,
        )
        .unwrap();
        let factory = fixture.factory();

        // A malformed route fails the syntax check.
        let error = validate_route(&factory, &workspace, "not-a-route")
            .await
            .unwrap_err();
        assert_eq!(error.kind, RunFailureKind::Configuration);
        assert!(
            error.message.contains("provider/model syntax"),
            "{}",
            error.message
        );

        // An unknown provider fails the provider check.
        let error = validate_route(&factory, &workspace, "ghost/model")
            .await
            .unwrap_err();
        assert!(
            error.message.contains("unknown or disabled provider"),
            "{}",
            error.message
        );

        // An unknown model id on a known provider is rejected — never
        // defaulted to the provider's API — and the rejection lists the
        // served routes while they are few.
        let error = validate_route(&factory, &workspace, "custom/typo")
            .await
            .unwrap_err();
        assert_eq!(error.kind, RunFailureKind::Configuration);
        assert!(
            error
                .message
                .contains(r#"model "typo" is not in provider "custom"'s authenticated model list"#),
            "{}",
            error.message
        );
        assert!(
            error.message.contains("available routes: custom/known"),
            "{}",
            error.message
        );

        // A catalog-less custom provider serves nothing without an
        // explicitly configured id...
        let error = validate_route(&factory, &workspace, "bare/anything")
            .await
            .unwrap_err();
        assert!(
            error.message.contains("authenticated model list"),
            "{}",
            error.message
        );
        assert!(
            !error.message.contains("available routes"),
            "{}",
            error.message
        );

        // ...while an explicitly configured id passes.
        validate_route(&factory, &workspace, "custom/known")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn spawn_validation_requires_provider_authentication_at_spawn_time() {
        let fixture = RuntimeFixture::new();
        let workspace = fs::canonicalize(fixture.path("work")).unwrap();
        let credentials = CredentialStore::with_backend(
            CredentialPaths::new(fixture.path("data")),
            Arc::new(MemoryKeyring::default()),
        );
        fs::write(
            fixture.path("global/config.ron"),
            r#"(
                version: 1,
                model: "openai/gpt-5.6",
                providers: {
                    "openai-codex": OpenAiCodex(
                        profile: "work",
                        models: {"gpt-test": (name: "Codex test")},
                    ),
                    "xai": XAi(profile: "work"),
                },
            )"#,
        )
        .unwrap();
        let factory = fixture.factory_with_credentials(credentials.clone());

        // A missing static key rejects the spawn even though the model id
        // is in the builtin catalog.
        let error = validate_route(&factory, &workspace, "openai/gpt-5.6")
            .await
            .unwrap_err();
        assert_eq!(
            error.kind,
            RunFailureKind::Authentication,
            "{}",
            error.message
        );
        assert!(
            error.message.contains("not authenticated"),
            "{}",
            error.message
        );

        // Request-time-auth providers must have resolvable credentials at
        // spawn time, not merely at first request.
        let error = validate_route(&factory, &workspace, "openai-codex/gpt-test")
            .await
            .unwrap_err();
        assert_eq!(error.kind, RunFailureKind::Authentication);
        let error = validate_route(&factory, &workspace, "xai/grok-4.5")
            .await
            .unwrap_err();
        assert_eq!(error.kind, RunFailureKind::Authentication);

        // Adding the static key makes the builtin-catalog route spawnable
        // without any discovery round trip.
        credentials
            .set("openai/default", "test-secret", false)
            .unwrap();
        validate_route(&factory, &workspace, "openai/gpt-5.6")
            .await
            .unwrap();

        // A stored Codex profile becomes resolvable and the configured
        // model id passes.
        let id_payload = serde_json::to_vec(&serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "workspace-test-id",
                "chatgpt_account_is_fedramp": false
            }
        }))
        .unwrap();
        let id_token = format!("e30.{}.signature", URL_SAFE_NO_PAD.encode(id_payload));
        let refreshed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let stored = serde_json::json!({
            "version": 1,
            "id_token": id_token,
            "access_token": "access-token",
            "refresh_token": "refresh-token",
            "account_id": "workspace-test-id",
            "is_fedramp": false,
            "refreshed_at": refreshed_at
        });
        credentials
            .set_with_metadata(
                "openai-codex/work",
                serde_json::to_vec(&stored).unwrap(),
                false,
                Some("openai-codex"),
                Some("https://chatgpt.com"),
            )
            .unwrap();
        validate_route(&factory, &workspace, "openai-codex/gpt-test")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn spawn_validation_rejects_policy_denied_providers() {
        let fixture = RuntimeFixture::new();
        let workspace = fs::canonicalize(fixture.path("work")).unwrap();
        fs::write(
            fixture.path("global/config.ron"),
            r#"(
                version: 1,
                model: "allowed/model",
                providers: {
                    "allowed": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {"model": (name: "Allowed model")},
                    ),
                    "denied": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {"model": (name: "Denied model")},
                    ),
                },
            )"#,
        )
        .unwrap();
        fs::write(
            fixture.path("managed/managed.ron"),
            r#"(version: 1, policy: (denied_providers: ["denied"]))"#,
        )
        .unwrap();
        let factory = fixture.factory();

        let error = validate_route(&factory, &workspace, "denied/model")
            .await
            .unwrap_err();
        assert_eq!(error.kind, RunFailureKind::Policy);
        assert!(error.message.contains("denied"), "{}", error.message);

        validate_route(&factory, &workspace, "allowed/model")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn spawn_validation_accepts_discovered_models_from_a_warm_cache_without_network() {
        use std::io::{Read as _, Write as _};

        let fixture = RuntimeFixture::new();
        let workspace = fs::canonicalize(fixture.path("work")).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let length = stream.read(&mut request).unwrap();
            let request = std::str::from_utf8(&request[..length]).unwrap();
            assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
            let body = r#"{"data":[{"id":"live-model","display_name":"Live model"}]}"#;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .unwrap();
        });
        fs::write(
            fixture.path("global/config.ron"),
            format!(
                r#"(
                    version: 1,
                    model: "custom/configured",
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://{address}/v1",
                                api: OpenAiResponses,
                                auth: NoAuth,
                            ),
                            models: {{"configured": (name: "Configured model")}},
                        ),
                    }},
                )"#
            ),
        )
        .unwrap();
        let factory = fixture.factory();

        // The first validation faults the discovery list into the cache and
        // accepts the discovered id — exactly what the served model list
        // would show.
        validate_route(&factory, &workspace, "custom/live-model")
            .await
            .unwrap();
        server.join().unwrap();

        // The endpoint is gone: a second validation can only succeed from
        // the warm cache, proving no network round trip is required.
        validate_route(&factory, &workspace, "custom/live-model")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn grant_authority_seeds_effective_grants_and_promotes_new_ones() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            // Trust state lives under the data directory, which the loader
            // requires to be private.
            fs::set_permissions(fixture.path("data"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::create_dir_all(fixture.path("work/.qq")).unwrap();
        fs::write(
            fixture.path("work/.qq/config.ron"),
            "(\n    version: 1,\n    model: \"openai/gpt-5.6\",\n    policy: (\n        allow_shell_prefixes: [\"cargo test\"],\n    ),\n)\n",
        )
        .unwrap();
        let workspace = fs::canonicalize(fixture.path("work")).unwrap();

        // Untrusted workspace grant declarations seed nothing: the trust
        // flow gates them exactly as it gates every sensitive declaration.
        let seed = WorkspaceGrantAuthority::seed_grants(&factory, &workspace).await;
        assert!(seed.is_empty());

        let request = LoadRequest::from_process_env(&workspace, None).unwrap();
        factory.inner.config.grant_pending_trust(&request).unwrap();
        let seed = WorkspaceGrantAuthority::seed_grants(&factory, &workspace).await;
        assert!(seed.shell_prefixes.contains(&"cargo test".to_owned()));
        // The compiled VCS read-only presets ride along with declared grants.
        assert!(seed.shell_prefixes.contains(&"git status".to_owned()));

        // Promotion writes the grant durably and reports the file; repeating
        // it is idempotent, and the next seed carries the promoted grant.
        let grant = qq_protocol::ApprovalGrant::Tool {
            name: "edit_file".to_owned(),
        };
        let outcome = WorkspaceGrantAuthority::promote_grant(&factory, &workspace, &grant).await;
        let WorkspaceGrantOutcome::Written { path } = outcome else {
            panic!("expected a written promotion, got {outcome:?}")
        };
        assert!(path.ends_with("config.ron"), "{path}");
        assert!(
            fs::read_to_string(fixture.path("work/.qq/config.ron"))
                .unwrap()
                .contains("edit_file")
        );
        let outcome = WorkspaceGrantAuthority::promote_grant(&factory, &workspace, &grant).await;
        assert!(matches!(
            outcome,
            WorkspaceGrantOutcome::AlreadyPresent { .. }
        ));
        let seed = WorkspaceGrantAuthority::seed_grants(&factory, &workspace).await;
        assert_eq!(seed.tools, ["edit_file"]);
        assert!(seed.shell_prefixes.contains(&"cargo test".to_owned()));

        // A managed deny refuses the promotion; the failure is data.
        fs::write(
            fixture.path("managed/managed.ron"),
            r#"(version: 1, policy: (deny_tools: ["mcp__executor__execute"]))"#,
        )
        .unwrap();
        let denied = qq_protocol::ApprovalGrant::Tool {
            name: "mcp__executor__execute".to_owned(),
        };
        let outcome = WorkspaceGrantAuthority::promote_grant(&factory, &workspace, &denied).await;
        assert!(matches!(outcome, WorkspaceGrantOutcome::Failed { .. }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn independent_factories_preserve_concurrent_workspace_grant_updates() {
        let fixture = RuntimeFixture::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(fixture.path("data"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        fs::create_dir_all(fixture.path("work")).unwrap();
        let workspace = fs::canonicalize(fixture.path("work")).unwrap();
        let start = Arc::new(tokio::sync::Barrier::new(3));

        let tool_factory = fixture.factory();
        let tool_workspace = workspace.clone();
        let tool_start = Arc::clone(&start);
        let tool = tokio::spawn(async move {
            tool_start.wait().await;
            WorkspaceGrantAuthority::promote_grant(
                &tool_factory,
                &tool_workspace,
                &ApprovalGrant::Tool {
                    name: "edit_file".to_owned(),
                },
            )
            .await
        });
        let shell_factory = fixture.factory();
        let shell_workspace = workspace.clone();
        let shell_start = Arc::clone(&start);
        let shell = tokio::spawn(async move {
            shell_start.wait().await;
            WorkspaceGrantAuthority::promote_grant(
                &shell_factory,
                &shell_workspace,
                &ApprovalGrant::ShellPrefix {
                    prefix: "cargo test".to_owned(),
                },
            )
            .await
        });
        start.wait().await;

        assert!(matches!(
            tool.await.unwrap(),
            WorkspaceGrantOutcome::Written { .. }
        ));
        assert!(matches!(
            shell.await.unwrap(),
            WorkspaceGrantOutcome::Written { .. }
        ));
        let content = fs::read_to_string(workspace.join(".qq/config.ron")).unwrap();
        assert!(content.contains("edit_file"), "{content}");
        assert!(content.contains("cargo test"), "{content}");
    }

    #[test]
    fn catalog_hides_builtin_models_until_the_provider_is_authenticated() {
        let fixture = RuntimeFixture::new();
        let credentials = CredentialStore::with_backend(
            CredentialPaths::new(fixture.path("data")),
            Arc::new(MemoryKeyring::default()),
        );
        let factory = fixture.factory_with_credentials(credentials.clone());
        let snapshot = factory
            .load(&fixture.request(r#"(version: 1, model: "openai/gpt-5.6")"#))
            .unwrap();

        assert!(factory.configured_model_options(&snapshot).is_empty());

        credentials
            .set("openai/default", "test-secret", false)
            .unwrap();
        let options = factory.configured_model_options(&snapshot);
        assert!(!options.is_empty());
        assert!(options.iter().all(|option| option.provider == "openai"));
        assert!(options.iter().any(|option| option.model == "gpt-5.6"));
        assert!(options.iter().any(|option| option.model == "gpt-6-astra"));
        assert!(
            options
                .iter()
                .find(|option| option.model == "gpt-5.6")
                .and_then(|option| option.context_window)
                .is_some()
        );
    }

    #[test]
    fn catalog_merges_live_ids_without_overriding_configured_metadata() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let snapshot = factory
            .load(&fixture.request(
                r#"(
                    version: 1,
                    model: "custom/configured",
                    providers: {
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: NoAuth,
                            ),
                            models: {"configured": (name: "Configured name")},
                        ),
                    },
                )"#,
            ))
            .unwrap();
        let discovered = BTreeMap::from([(
            "custom".to_owned(),
            vec![
                DiscoveredModel {
                    id: "configured".to_owned(),
                    name: Some("Vendor name".to_owned()),
                },
                DiscoveredModel {
                    id: "live".to_owned(),
                    name: Some("Live name".to_owned()),
                },
            ],
        )]);

        let options = factory.model_options_with_discovery(&snapshot, &discovered);

        assert!(options.iter().any(|option| {
            option.model == "configured" && option.name.as_deref() == Some("Configured name")
        }));
        assert!(options
            .iter()
            .any(|option| option.model == "live" && option.name.as_deref() == Some("Live name")));
    }

    #[test]
    fn constructs_every_wired_http_api_and_builtin_key_provider() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();

        for api in [
            "OpenAiResponses",
            "OpenAiChatCompletions",
            "AnthropicMessages",
            "GoogleGenerateContent",
        ] {
            let request = fixture.request(format!(
                r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: {api},
                                auth: NoAuth,
                            ),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
            ));
            factory
                .plan_for(&request)
                .unwrap_or_else(|error| panic!("failed to construct {api}: {error}"));
        }

        let anthropic = fixture.request(
            r#"(
                version: 1,
                model: "anthropic/claude-test",
                providers: {
                    "anthropic": Anthropic(
                        api_key: Value("anthropic-test-secret"),
                        models: {"claude-test": (name: "Claude test")},
                    ),
                },
            )"#,
        );
        factory.plan_for(&anthropic).unwrap();

        let google = fixture.request(
            r#"(
                version: 1,
                model: "google/gemini-test",
                providers: {
                    "google": Google(
                        api_key: Value("google-test-secret"),
                        models: {"gemini-test": (name: "Gemini test")},
                    ),
                },
            )"#,
        );
        factory.plan_for(&google).unwrap();
    }

    #[test]
    fn constructs_xai_runtimes_for_model_selected_responses_and_chat_protocols() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();

        for model in ["grok-4.6", "grok-4.5", "grok-4.3"] {
            let request = fixture.request(format!(
                r#"(
                    version: 1,
                    model: "xai/{model}",
                    providers: {{
                        "xai": XAi(api_key: Value("xai-test-secret")),
                    }},
                )"#
            ));
            factory.plan_for(&request).unwrap();
        }
    }

    #[test]
    fn accepts_case_insensitive_loopback_http_schemes() {
        let fixture = RuntimeFixture::new();
        let request = fixture.request(
            r#"(
                version: 1,
                model: "custom/test-model",
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "HTTP://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {"test-model": (name: "Test model")},
                    ),
                },
            )"#,
        );

        fixture.factory().plan_for(&request).unwrap();
    }

    #[test]
    fn constructs_and_reuses_openai_codex_runtime_for_the_selected_profile() {
        let fixture = RuntimeFixture::new();
        let credentials = CredentialStore::with_backend(
            CredentialPaths::new(fixture.path("data")),
            Arc::new(MemoryKeyring::default()),
        );
        let id_payload = serde_json::to_vec(&serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "workspace-test-id",
                "chatgpt_account_is_fedramp": false
            }
        }))
        .unwrap();
        let id_token = format!("e30.{}.signature", URL_SAFE_NO_PAD.encode(id_payload));
        let refreshed_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let stored = serde_json::json!({
            "version": 1,
            "id_token": id_token,
            "access_token": "access-token",
            "refresh_token": "refresh-token",
            "account_id": "workspace-test-id",
            "is_fedramp": false,
            "refreshed_at": refreshed_at
        });
        credentials
            .set_with_metadata(
                "openai-codex/work",
                serde_json::to_vec(&stored).unwrap(),
                false,
                Some("openai-codex"),
                Some("https://chatgpt.com"),
            )
            .unwrap();
        let factory = fixture.factory_with_credentials(credentials);
        let request = fixture.request(
            r#"(
                version: 1,
                model: "openai-codex/gpt-test",
                providers: {
                    "openai-codex": OpenAiCodex(
                        profile: "work",
                        models: {"gpt-test": (name: "Codex test")},
                    ),
                },
            )"#,
        );

        let first = factory.plan_for(&request).unwrap();
        let second = factory.plan_for(&request).unwrap();

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[test]
    fn runtime_cache_identity_includes_effective_context_window() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let request = |context_window: u32| {
            fixture.request(format!(
                r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: NoAuth,
                            ),
                            models: {{
                                "test-model": (
                                    name: "Test model",
                                    context_window: {context_window},
                                ),
                            }},
                        ),
                    }},
                )"#,
            ))
        };

        let first = factory.plan_for(&request(32_768)).unwrap();
        let reused = factory.plan_for(&request(32_768)).unwrap();
        let changed = factory.plan_for(&request(65_536)).unwrap();

        assert!(Arc::ptr_eq(&first, &reused));
        assert!(!Arc::ptr_eq(&first, &changed));
    }

    #[tokio::test]
    async fn direct_runtime_uses_catalog_context_window_before_provider_io() {
        use futures_util::StreamExt as _;

        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let request = fixture.request(
            r#"(
                version: 1,
                model: "custom/test-model",
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {
                            "test-model": (
                                name: "Test model",
                                context_window: 1,
                            ),
                        },
                    ),
                },
            )"#,
        );
        let plan = factory.plan_for(&request).unwrap();
        let mut events = plan.run(qq_protocol::RunCommand::new("this cannot fit"));
        let mut failure = None;
        while let Some(event) = events.next().await {
            if let qq_protocol::RunEvent::Failed { kind, message } = event {
                failure = Some((kind, message));
                break;
            }
        }

        let (kind, message) = failure.expect("the context plan must fail before provider I/O");
        assert_eq!(kind, RunFailureKind::Policy);
        assert!(message.contains("context"), "{message}");
    }

    #[test]
    fn constructs_amazon_bedrock_runtimes_for_every_auth_mode_without_network_access() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();

        for (provider, auth) in [
            ("bedrock-default", "Aws(DefaultChain)"),
            ("bedrock-profile", r#"Aws(Profile("work"))"#),
            ("bedrock-api-key", r#"ApiKey(Value("bedrock-test-secret"))"#),
        ] {
            let request = fixture.request(format!(
                r#"(
                    version: 1,
                    model: "{provider}/test-model",
                    providers: {{
                        "{provider}": AmazonBedrock(
                            region: "us-east-1",
                            auth: {auth},
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
            ));

            factory
                .plan_for(&request)
                .unwrap_or_else(|error| panic!("failed to construct {provider}: {error}"));
        }
    }

    #[test]
    fn constructs_amazon_bedrock_mantle_runtimes_for_supported_apis_and_auth_modes() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();

        for api in [
            "OpenAiResponses",
            "OpenAiChatCompletions",
            "AnthropicMessages",
        ] {
            for (auth_name, auth) in [
                ("default", "Aws(DefaultChain)"),
                ("profile", r#"Aws(Profile("work"))"#),
                ("api-key", r#"ApiKey(Value("mantle-test-secret"))"#),
            ] {
                let provider = format!("mantle-{api}-{auth_name}");
                let request = fixture.request(format!(
                    r#"(
                        version: 1,
                        model: "{provider}/test-model",
                        providers: {{
                            "{provider}": AmazonBedrockMantle(
                                region: "us-east-1",
                                api: {api},
                                auth: {auth},
                                models: {{"test-model": (name: "Test model")}},
                            ),
                        }},
                    )"#
                ));

                factory.plan_for(&request).unwrap_or_else(|error| {
                    panic!("failed to construct Mantle {api}/{auth_name}: {error}")
                });
            }
        }
    }

    #[test]
    fn rejects_unsupported_amazon_bedrock_mantle_apis_before_network_access() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();

        for (api, expected) in [
            ("GoogleGenerateContent", ProviderApi::GoogleGenerateContent),
            ("BedrockConverse", ProviderApi::BedrockConverse),
        ] {
            let request = fixture.request(format!(
                r#"(
                    version: 1,
                    model: "mantle/test-model",
                    providers: {{
                        "mantle": AmazonBedrockMantle(
                            region: "us-east-1",
                            api: {api},
                            auth: Aws(DefaultChain),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
            ));

            let error = factory
                .plan_for(&request)
                .expect_err("unsupported Mantle API must fail");
            assert!(matches!(
                error,
                RuntimeBuildError::UnsupportedApi { api: actual, .. }
                    if actual == expected
            ));
        }
    }

    #[test]
    fn mantle_runtime_cache_identity_includes_region_api_and_aws_profile() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let document = |region: &str, api: &str, auth: &str| {
            fixture.request(format!(
                r#"(
                    version: 1,
                    model: "mantle/test-model",
                    providers: {{
                        "mantle": AmazonBedrockMantle(
                            region: "{region}",
                            api: {api},
                            auth: {auth},
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
            ))
        };

        let base = document("us-east-1", "OpenAiResponses", "Aws(DefaultChain)");
        let first = factory.plan_for(&base).unwrap();
        let reused = factory.plan_for(&base).unwrap();
        let different_region = factory
            .plan_for(&document(
                "us-west-2",
                "OpenAiResponses",
                "Aws(DefaultChain)",
            ))
            .unwrap();
        let different_api = factory
            .plan_for(&document(
                "us-east-1",
                "AnthropicMessages",
                "Aws(DefaultChain)",
            ))
            .unwrap();
        let different_profile = factory
            .plan_for(&document(
                "us-east-1",
                "OpenAiResponses",
                r#"Aws(Profile("work"))"#,
            ))
            .unwrap();

        assert!(Arc::ptr_eq(&first, &reused));
        assert!(!Arc::ptr_eq(&first, &different_region));
        assert!(!Arc::ptr_eq(&first, &different_api));
        assert!(!Arc::ptr_eq(&first, &different_profile));
    }

    #[test]
    fn mantle_model_api_metadata_overrides_the_provider_default() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let document = |model: &str| {
            fixture.request(format!(
                r#"(
                    version: 1,
                    model: "mantle/{model}",
                    providers: {{
                        "mantle": AmazonBedrockMantle(
                            region: "us-east-1",
                            api: AnthropicMessages,
                            auth: Aws(DefaultChain),
                            models: {{
                                "openai-model": (name: "OpenAI model", api: OpenAiResponses),
                                "rejected-model": (name: "Rejected model", api: BedrockConverse),
                                "anthropic-model": (name: "Anthropic model"),
                            }},
                        ),
                    }},
                )"#
            ))
        };

        factory
            .plan_for(&document("openai-model"))
            .expect("per-model OpenAiResponses override must construct");
        factory
            .plan_for(&document("anthropic-model"))
            .expect("provider default AnthropicMessages must construct");

        // The per-model API must reach Mantle preparation: an unsupported
        // override fails even though the provider default is supported.
        let error = factory
            .plan_for(&document("rejected-model"))
            .expect_err("unsupported per-model API must fail");
        assert!(matches!(
            error,
            RuntimeBuildError::UnsupportedApi {
                api: ProviderApi::BedrockConverse,
                ..
            }
        ));
    }

    #[test]
    fn reuses_matching_runtimes_and_separates_auth_modes() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let document = |auth: &str| {
            format!(
                r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: {auth},
                            ),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
            )
        };

        let api_key = fixture.request(document(r#"ApiKey(Value("same-test-secret"))"#));
        let bearer = fixture.request(document(r#"Bearer(Value("same-test-secret"))"#));
        let first = factory.plan_for(&api_key).unwrap();
        let reused = factory.plan_for(&api_key).unwrap();
        let different_auth = factory.plan_for(&bearer).unwrap();

        assert!(Arc::ptr_eq(&first, &reused));
        assert!(!Arc::ptr_eq(&first, &different_auth));
    }

    #[test]
    fn plan_digest_includes_custom_auth_header_name_but_never_its_value() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let document = |header: &str, secret: &str| {
            format!(
                r#"(
                    version: 1,
                    model: "custom/test-model",
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: Header("{header}", Value("{secret}")),
                            ),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
            )
        };

        let first = factory
            .plan_for(&fixture.request(document("x-first", "same-test-secret")))
            .unwrap();
        let second = factory
            .plan_for(&fixture.request(document("x-second", "same-test-secret")))
            .unwrap();
        let rotated = factory
            .plan_for(&fixture.request(document("x-first", "rotated-test-secret")))
            .unwrap();

        assert_ne!(first.digest(), second.digest());
        // The header value is a secret: rotating it must not change behavior.
        assert_eq!(first.digest(), rotated.digest());
        assert_eq!(first.descriptor().provider.auth_scheme, "header:x-first");
        assert_eq!(
            first.descriptor().provider.credential,
            CredentialReference::Inline
        );
        let canonical = String::from_utf8(first.descriptor().canonical_bytes().unwrap()).unwrap();
        assert!(!canonical.contains("same-test-secret"));
        assert!(!format!("{first:?}").contains("same-test-secret"));
    }

    #[test]
    fn credential_rotation_changes_the_epoch_but_not_the_plan_digest() {
        let fixture = RuntimeFixture::new();
        let credentials = CredentialStore::with_backend(
            CredentialPaths::new(fixture.path("data")),
            Arc::new(MemoryKeyring::default()),
        );
        credentials
            .set("openai/default", "sk-first-secret", false)
            .unwrap();
        let factory = fixture.factory_with_credentials(credentials.clone());
        let request = fixture.request(r#"(version: 1, model: "openai/gpt-5.6")"#);

        let (first, lookup) = factory
            .plan_with_lookup(&request, &AgentProfileId::default())
            .unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
        let (hit, lookup) = factory
            .plan_with_lookup(&request, &AgentProfileId::default())
            .unwrap();
        assert_eq!(lookup, PlanLookup::Hit);
        assert!(Arc::ptr_eq(&first, &hit));

        credentials
            .set("openai/default", "sk-second-secret", false)
            .unwrap();
        let (rotated, lookup) = factory
            .plan_with_lookup(&request, &AgentProfileId::default())
            .unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
        assert!(!Arc::ptr_eq(&first, &rotated));
        assert_eq!(first.digest(), rotated.digest());
        assert!(rotated.credential_epoch() > first.credential_epoch());
        assert_eq!(
            rotated.descriptor().provider.credential,
            CredentialReference::Stored("openai/default".to_owned())
        );
        for plan in [&first, &rotated] {
            let canonical =
                String::from_utf8(plan.descriptor().canonical_bytes().unwrap()).unwrap();
            assert!(!canonical.contains("sk-first-secret"));
            assert!(!canonical.contains("sk-second-secret"));
            let debug = format!("{plan:?}");
            assert!(!debug.contains("sk-first-secret"));
            assert!(!debug.contains("sk-second-secret"));
        }
    }

    #[test]
    fn configured_static_exposure_names_match_the_compiled_catalog() {
        let fixture = RuntimeFixture::new();
        let skill = fixture.path("work/.qq/skills/example/SKILL.md");
        fs::create_dir_all(skill.parent().unwrap()).unwrap();
        fs::write(
            skill,
            "---\ndescription: Example guidance\n---\nBe brief.\n",
        )
        .unwrap();
        let document = |policy: &str| {
            format!(
                r#"(version: 1, model: "custom/test-model", providers: {{
            "custom": Custom(connection: (base_url: "http://localhost/v1", api: OpenAiChatCompletions, auth: NoAuth),
            models: {{"test-model": (name: "Test model")}})
        }}, policy: ({policy}))"#
            )
        };
        let factory = fixture.factory();
        let default = factory.plan_for(&fixture.request(document(""))).unwrap();
        let names: Vec<_> = default.catalog().names().map(str::to_owned).collect();
        assert!(names.iter().any(|name| name == "load_skill"));
        for name in &names {
            let request = fixture.request(document(&format!("exposed_tools: [{name:?}]")));
            let snapshot = factory.load(&request).unwrap();
            assert_eq!(
                snapshot.policy().exposed_tools().unwrap(),
                std::slice::from_ref(name)
            );
        }
        let list = serde_json::to_string(&names).unwrap();
        let all = factory
            .plan_for(&fixture.request(document(&format!("exposed_tools: {list}"))))
            .unwrap();
        assert_eq!(default.digest(), all.digest());
        let narrow = factory
            .plan_for(&fixture.request(document(r#"exposed_tools: ["read_file", "search"]"#)))
            .unwrap();
        assert_eq!(
            narrow.catalog().names().collect::<Vec<_>>(),
            ["read_file", "search"]
        );
        let missing =
            factory.plan_for(&fixture.request(document(r#"exposed_tools: ["mcp__absent__tool"]"#)));
        assert!(matches!(
            missing,
            Err(RuntimeBuildError::Plan(
                PlanCompileError::UnknownExposedTool { .. }
            ))
        ));
    }

    #[test]
    fn exposure_intersects_profile_mcp_admission_before_member_validation() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(fixture.path("data"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let manifest = fixture.path("work/.qq/packs/review/pack.ron");
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(
            manifest,
            r#"(schema: 1, id: "review", version: "1.0.0", profiles: {
            "local": Profile(mcp: []),
            "remote": Profile(mcp: ["notes"]),
        })"#,
        )
        .unwrap();
        let document = |exposure: &str| {
            format!(
                r#"(version: 1, model: "custom/test-model", providers: {{
            "custom": Custom(connection: (base_url: "http://localhost/v1", api: OpenAiChatCompletions, auth: NoAuth),
                models: {{"test-model": (name: "Test")}})
        }}, mcp: {{"notes": Stdio(command: "qq-test-must-not-start")}},
            policy: (exposed_tools: ["read_file", "{exposure}"]))"#
            )
        };
        let request = fixture.request(document("mcp__notes__missing"));
        factory.inner.config.grant_pending_trust(&request).unwrap();
        let local = AgentProfileId::new("local").unwrap();
        let plan = factory.plan_for_profile(&request, &local).unwrap();
        assert_eq!(plan.catalog().names().collect::<Vec<_>>(), ["read_file"]);
        assert!(plan.descriptor().mcp_servers.is_empty());
        let remote = AgentProfileId::new("remote").unwrap();
        for (request, profile, expected) in [
            (request, remote, "mcp__notes__missing"),
            (
                fixture.request(document("mcp__typo__missing")),
                local,
                "mcp__typo__missing",
            ),
        ] {
            let error = factory.plan_for_profile(&request, &profile).unwrap_err();
            assert!(
                matches!(error, RuntimeBuildError::Plan(PlanCompileError::UnknownExposedTool { name }) if name == expected)
            );
        }
    }

    #[test]
    fn source_changes_during_plan_admission_do_not_certify_stale_configuration() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        let config = fixture.path("global/config.ron");
        let document = |cap: u32| {
            format!(
                r#"(version: 1, model: "custom/test-model", max_output_tokens: {cap}, providers: {{
            "custom": Custom(connection: (base_url: "http://localhost/v1", api: OpenAiChatCompletions, auth: NoAuth),
            models: {{"test-model": (name: "Test model")}})
        }})"#
            )
        };
        fs::write(&config, document(128)).unwrap();
        let request = LoadRequest::new(fixture.path("work"));
        let workspace = qq_config::canonical_working_directory(request.cwd()).unwrap();
        let profile = AgentProfileId::default();
        let key = PlanKey {
            workspace: workspace.clone(),
            model: ModelSelection::default(),
            profile: profile.clone(),
            explicit_config_path: None,
            explicit_config_content: None,
        };
        let (first, _) = factory
            .inner
            .plans
            .load::<RuntimeBuildError, _>(key, || {
                let generation = factory.compile_generation(&request, &profile, &workspace)?;
                fs::write(&config, document(1024)).unwrap();
                Ok(generation)
            })
            .unwrap();
        let (next, lookup) = factory.plan_with_lookup(&request, &profile).unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
        assert!(!Arc::ptr_eq(&first, &next));
        assert_ne!(first.digest(), next.digest());
        assert_eq!(
            factory.plan_with_lookup(&request, &profile).unwrap().1,
            PlanLookup::Hit
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn file_backed_provider_rotation_rebinds_new_runs_and_preserves_held_plans() {
        use futures_util::StreamExt as _;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for _ in 0..6 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                loop {
                    let mut chunk = [0; 4096];
                    let count = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(count, 0);
                    bytes.extend_from_slice(&chunk[..count]);
                    assert!(bytes.len() < 256 * 1024);
                    let Some(end) = bytes.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse::<usize>().unwrap())
                        })
                        .unwrap();
                    if bytes.len() >= end + 4 + length {
                        requests.push(headers.to_ascii_lowercase());
                        break;
                    }
                }
                let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pong\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":1}}\n\ndata: [DONE]\n\n";
                socket.write_all(format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
                ).as_bytes()).await.unwrap();
            }
            requests
        });

        for kind in ["api-key", "auth-header", "static-header"] {
            let (fixture, first, rotated) = tokio::task::spawn_blocking(move || {
                let fixture = RuntimeFixture::new();
                let factory = fixture.factory();
                let config = fixture.path("global/config.ron");
                let request = LoadRequest::new(fixture.path("work"));
                let document = |value: &str| {
                    let access = match kind {
                        "api-key" => format!(r#"auth: ApiKey(Value("{value}"))"#),
                        "auth-header" => {
                            format!(r#"auth: Header("x-test-token", Value("{value}"))"#)
                        }
                        "static-header" => {
                            format!(r#"auth: NoAuth, headers: {{"x-test-token": "{value}"}}"#)
                        }
                        _ => unreachable!(),
                    };
                    format!(
                        r#"(version: 1, model: "custom/test-model", providers: {{
                        "custom": Custom(connection: (base_url: "http://{address}/v1",
                            api: OpenAiChatCompletions, {access}),
                            models: {{"test-model": (name: "Test model")}})
                    }})"#
                    )
                };
                fs::write(&config, document("first-inline-value")).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
                }
                let first = factory.plan_for(&request).unwrap();
                fs::write(&config, document("second-longer-inline-value")).unwrap();
                let (rotated, lookup) = factory
                    .plan_with_lookup(&request, &AgentProfileId::default())
                    .unwrap();
                assert_eq!(
                    lookup,
                    PlanLookup::Compiled,
                    "{kind} must refresh its live binding"
                );
                assert!(!Arc::ptr_eq(&first, &rotated));
                assert_eq!(first.digest(), rotated.digest());
                assert_eq!(first.credential_epoch(), rotated.credential_epoch());
                for plan in [&first, &rotated] {
                    let descriptor =
                        String::from_utf8(plan.descriptor().canonical_bytes().unwrap()).unwrap();
                    let debug = format!("{plan:?}");
                    for secret in ["first-inline-value", "second-longer-inline-value"] {
                        assert!(!descriptor.contains(secret));
                        assert!(!debug.contains(secret));
                    }
                }
                fs::write(
                    &config,
                    format!(
                        "{}\n// unchanged live configuration\n",
                        document("second-longer-inline-value")
                    ),
                )
                .unwrap();
                let (revalidated, lookup) = factory
                    .plan_with_lookup(&request, &AgentProfileId::default())
                    .unwrap();
                assert_eq!(lookup, PlanLookup::Revalidated);
                assert!(Arc::ptr_eq(&rotated, &revalidated));
                (fixture, first, rotated)
            })
            .await
            .unwrap();
            for plan in [first, rotated] {
                let events = tokio::time::timeout(
                    Duration::from_secs(5),
                    plan.run(qq_protocol::RunCommand::new("reply pong"))
                        .collect::<Vec<_>>(),
                )
                .await
                .unwrap();
                assert!(
                    matches!(events.last(), Some(qq_protocol::RunEvent::Completed)),
                    "{events:?}"
                );
            }
            drop(fixture);
        }
        let requests = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        for (index, header) in ["authorization: bearer", "x-test-token:", "x-test-token:"]
            .into_iter()
            .enumerate()
        {
            assert!(requests[index * 2].contains(&format!("{header} first-inline-value")));
            assert!(
                requests[index * 2 + 1].contains(&format!("{header} second-longer-inline-value"))
            );
        }
    }

    #[test]
    fn profiles_select_defaults_key_the_plan_cache_and_reject_unknown_names() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(fixture.path("data"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let config = fixture.path("work/.qq/config.ron");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            r#"(
                version: 1,
                model: "custom/test-model",
                max_output_tokens: 64,
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: {
                            "test-model": (name: "Test model"),
                            "big-model": (name: "Big model"),
                        },
                    ),
                },
                profiles: {
                    "review": Profile(model: "custom/big-model", approval_mode: read_only),
                    "tight": Profile(max_output_tokens: 16),
                },
            )"#,
        )
        .unwrap();
        let request = LoadRequest::new(fixture.path("work"));
        factory.inner.config.grant_pending_trust(&request).unwrap();

        let review = AgentProfileId::new("review").unwrap();
        let (default_plan, lookup) = factory
            .plan_with_lookup(&request, &AgentProfileId::default())
            .unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
        let (review_plan, lookup) = factory.plan_with_lookup(&request, &review).unwrap();
        // A different profile is a different cache slot and a different plan.
        assert_eq!(lookup, PlanLookup::Compiled);
        assert_ne!(review_plan.digest(), default_plan.digest());
        assert_eq!(review_plan.resolved_model().provider_model, "big-model");
        assert_eq!(review_plan.resolved_model().max_output_tokens, 64);
        assert_eq!(review_plan.identity().profile, review);
        assert_eq!(
            factory.plan_with_lookup(&request, &review).unwrap().1,
            PlanLookup::Hit
        );
        assert_eq!(
            factory
                .plan_with_lookup(&request, &AgentProfileId::default())
                .unwrap()
                .1,
            PlanLookup::Hit
        );

        // Profile values sit beneath explicit request overrides.
        let tight = AgentProfileId::new("tight").unwrap();
        let tight_plan = factory.plan_for_profile(&request, &tight).unwrap();
        assert_eq!(tight_plan.resolved_model().max_output_tokens, 16);
        let overridden = request
            .clone()
            .with_overrides(request.overrides().clone().with_max_output_tokens(32));
        let overridden_plan = factory.plan_for_profile(&overridden, &tight).unwrap();
        assert_eq!(overridden_plan.resolved_model().max_output_tokens, 32);

        let unknown = AgentProfileId::new("nope").unwrap();
        let error = factory.plan_for_profile(&request, &unknown).unwrap_err();
        assert!(matches!(&error, RuntimeBuildError::UnknownProfile(name) if *name == unknown));
        assert_eq!(error.failure_kind(), RunFailureKind::Configuration);

        // The capability document lists default first with effective values.
        let profiles = factory
            .profiles_for(fixture.path("work").to_str().unwrap())
            .unwrap();
        assert_eq!(
            profiles
                .iter()
                .map(|profile| profile.id.as_str())
                .collect::<Vec<_>>(),
            ["default", "review", "tight"]
        );
        assert_eq!(profiles[0].model.as_deref(), Some("custom/test-model"));
        assert_eq!(profiles[0].approval_mode, ApprovalMode::Auto);
        assert_eq!(profiles[1].model.as_deref(), Some("custom/big-model"));
        assert_eq!(profiles[1].approval_mode, ApprovalMode::ReadOnly);
        assert_eq!(profiles[2].model.as_deref(), Some("custom/test-model"));
    }

    #[test]
    fn pack_profiles_compile_persona_skills_tool_policy_and_mcp_subsets_into_plans() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(fixture.path("data"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let config = fixture.path("work/.qq/config.ron");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            r#"(
                version: 1,
                model: "custom/test-model",
                max_output_tokens: 64,
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: NoAuth,
                        ),
                        models: { "test-model": (name: "Test model") },
                    ),
                },
                mcp: {
                    "notes": Stdio(command: "qq-test-no-such-notes"),
                    "tickets": Stdio(command: "qq-test-no-such-tickets"),
                },
            )"#,
        )
        .unwrap();
        for (path, content) in [
            (
                "work/.qq/packs/review-kit/pack.ron",
                r#"(
                    schema: 1,
                    id: "review-kit",
                    version: "1.0.0",
                    profiles: {
                        "reviewer": Profile(
                            approval_mode: read_only,
                            prompt: "persona.md",
                            skills: ["skills"],
                            tools: (deny: ["shell", "write_file"]),
                            mcp: ["notes"],
                        ),
                    },
                )"#,
            ),
            ("work/.qq/packs/review-kit/persona.md", "Persona v1.\n"),
            (
                "work/.qq/packs/review-kit/skills/audit/SKILL.md",
                "---\ndescription: Audit it\n---\nbody\n",
            ),
        ] {
            let path = fixture.path(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, content).unwrap();
        }
        let request = LoadRequest::new(fixture.path("work"));
        factory.inner.config.grant_pending_trust(&request).unwrap();

        let reviewer = AgentProfileId::new("reviewer").unwrap();
        let (plan, lookup) = factory.plan_with_lookup(&request, &reviewer).unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
        let descriptor = plan.descriptor();
        let pack = descriptor.pack.as_ref().unwrap();
        assert_eq!(
            (pack.id.as_str(), pack.version.as_str()),
            ("review-kit", "1.0.0")
        );
        assert_eq!(pack.tool_deny, ["shell", "write_file"]);
        assert!(pack.persona_hash.is_some());
        // Only the referenced MCP server joins this plan.
        assert_eq!(descriptor.mcp_servers.len(), 1);
        assert_eq!(descriptor.mcp_servers[0].name, "notes");
        let names: Vec<&str> = plan.catalog().names().collect();
        assert!(!names.contains(&"shell") && !names.contains(&"write_file"));
        assert!(names.contains(&"edit_file") && names.contains(&"load_skill"));
        assert_eq!(plan.skills().disclosed_count(), 1);
        assert_eq!(
            plan.skills()
                .entries()
                .iter()
                .find(|e| e.name == "audit")
                .unwrap()
                .source,
            "pack:review-kit/skills/audit/SKILL.md"
        );
        // The default profile sees both servers and every static tool.
        let default_plan = factory.plan_for(&request).unwrap();
        assert_eq!(default_plan.descriptor().mcp_servers.len(), 2);
        assert!(default_plan.descriptor().pack.is_none());
        assert!(default_plan.catalog().names().any(|n| n == "shell"));

        // Warm lookups do no discovery and hit the cache.
        assert_eq!(
            factory.plan_with_lookup(&request, &reviewer).unwrap().1,
            PlanLookup::Hit
        );
        // Editing the persona recompiles; the old plan stays valid for holders.
        fs::write(
            fixture.path("work/.qq/packs/review-kit/persona.md"),
            "Persona v2.\n",
        )
        .unwrap();
        let (edited, lookup) = factory.plan_with_lookup(&request, &reviewer).unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
        assert_ne!(edited.digest(), plan.digest());
        assert_eq!(plan.descriptor().pack.as_ref().unwrap().version, "1.0.0");

        // The capability document names the pack behind the profile.
        let profiles = factory
            .profiles_for(fixture.path("work").to_str().unwrap())
            .unwrap();
        let summary = profiles.iter().find(|p| p.id == reviewer).unwrap();
        assert_eq!(summary.pack.as_ref().unwrap().id, "review-kit");
        assert_eq!(summary.approval_mode, ApprovalMode::ReadOnly);

        // A pack that needs a newer protocol fails as configuration.
        fs::write(
            fixture.path("work/.qq/packs/review-kit/pack.ron"),
            r#"(schema: 1, id: "review-kit", version: "9", requires: (protocol: 999),
                profiles: { "reviewer": Profile(max_output_tokens: 8) })"#,
        )
        .unwrap();
        let error = factory.plan_for_profile(&request, &reviewer).unwrap_err();
        assert!(matches!(
            error,
            RuntimeBuildError::PackRequiresNewerProtocol { required: 999, .. }
        ));
        assert_eq!(error.failure_kind(), RunFailureKind::Configuration);
    }

    #[test]
    fn workspace_config_edits_recompile_and_a_broken_edit_keeps_the_valid_plan() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // Trust state lives under the data directory, which must be private.
            fs::set_permissions(fixture.path("data"), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let config = fixture.path("work/.qq/config.ron");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let document = |max_output_tokens: u32| {
            format!(
                r#"(
                    version: 1,
                    model: "custom/test-model",
                    max_output_tokens: {max_output_tokens},
                    providers: {{
                        "custom": Custom(
                            connection: (
                                base_url: "http://127.0.0.1:1/v1",
                                api: OpenAiResponses,
                                auth: NoAuth,
                            ),
                            models: {{"test-model": (name: "Test model")}},
                        ),
                    }},
                )"#
            )
        };
        fs::write(&config, document(64)).unwrap();
        // No explicit content: the project file is the source under test.
        let request = LoadRequest::new(fixture.path("work"));
        factory.inner.config.grant_pending_trust(&request).unwrap();

        let (first, lookup) = factory
            .plan_with_lookup(&request, &AgentProfileId::default())
            .unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
        assert_eq!(first.resolved_model().max_output_tokens, 64);
        assert_eq!(
            factory
                .plan_with_lookup(&request, &AgentProfileId::default())
                .unwrap()
                .1,
            PlanLookup::Hit
        );

        std::thread::sleep(Duration::from_millis(20));
        fs::write(&config, document(96)).unwrap();
        factory.inner.config.grant_pending_trust(&request).unwrap();
        let (edited, lookup) = factory
            .plan_with_lookup(&request, &AgentProfileId::default())
            .unwrap();
        assert_eq!(lookup, PlanLookup::Compiled);
        assert_eq!(edited.resolved_model().max_output_tokens, 96);
        assert_ne!(first.digest(), edited.digest());

        std::thread::sleep(Duration::from_millis(20));
        fs::write(&config, "(version: 1, model: ").unwrap();
        assert!(
            factory
                .plan_with_lookup(&request, &AgentProfileId::default())
                .is_err()
        );
        // The previous valid generation survives the failed refresh.
        std::thread::sleep(Duration::from_millis(20));
        fs::write(&config, document(96)).unwrap();
        let (recovered, _) = factory
            .plan_with_lookup(&request, &AgentProfileId::default())
            .unwrap();
        assert_eq!(recovered.digest(), edited.digest());
    }

    #[test]
    fn plan_descriptor_covers_provider_model_workspace_and_tools_without_secrets() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();
        fs::write(fixture.path("work/AGENTS.md"), "Always answer in haiku.\n").unwrap();
        let request = fixture.request(
            r#"(
                version: 1,
                model: "custom/test-model",
                providers: {
                    "custom": Custom(
                        connection: (
                            base_url: "http://127.0.0.1:1/v1",
                            api: OpenAiResponses,
                            auth: Bearer(Value("sk-live-sentinel")),
                            headers: {"X-Static": "static-value"},
                        ),
                        models: {"test-model": (name: "Test model", context_window: 4096)},
                    ),
                },
            )"#,
        );

        let plan = factory.plan_for(&request).unwrap();
        let descriptor = plan.descriptor();
        assert_eq!(descriptor.provider.id, "custom");
        assert_eq!(descriptor.provider.api, "openai_responses");
        assert_eq!(
            descriptor.provider.endpoint.as_deref(),
            Some("http://127.0.0.1:1/v1")
        );
        assert_eq!(descriptor.provider.endpoint_mode.as_deref(), Some("base"));
        assert_eq!(descriptor.provider.auth_scheme, "bearer");
        assert_eq!(descriptor.provider.credential, CredentialReference::Inline);
        assert_eq!(
            descriptor.provider.header_names,
            vec!["X-Static".to_owned()]
        );
        assert_eq!(descriptor.model.context_window, Some(4096));
        assert_eq!(
            descriptor.workspace,
            fixture.path("work").display().to_string()
        );
        assert_eq!(descriptor.instruction_source.as_deref(), Some("AGENTS.md"));
        assert!(descriptor.tools.names.contains(&"read_file".to_owned()));
        assert!(descriptor.tools.names.contains(&"spawn_agent".to_owned()));
        assert!(
            descriptor
                .tools
                .names
                .contains(&"search_history".to_owned())
        );
        assert!(!descriptor.provenance.is_empty());

        let canonical = String::from_utf8(descriptor.canonical_bytes().unwrap()).unwrap();
        for forbidden in ["static-value", "sk-live-sentinel"] {
            assert!(
                !canonical.contains(forbidden),
                "descriptor leaked {forbidden}"
            );
        }
        assert!(canonical.starts_with("qq-agent-plan-descriptor-v5\0{"));
    }

    #[test]
    fn described_endpoints_drop_userinfo_query_and_fragment() {
        assert_eq!(
            describe_endpoint(
                "https://user:hunter2@api.example.test:8443/v1/responses?token=abc#frag"
            ),
            "https://api.example.test:8443/v1/responses"
        );
        assert_eq!(
            describe_endpoint("http://127.0.0.1:1/v1"),
            "http://127.0.0.1:1/v1"
        );
        assert_eq!(describe_endpoint("not a url"), "unparseable");
        assert_eq!(describe_endpoint("http://exa mple"), "http://<unparseable>");
    }

    /// Not a correctness test: prints cold-compile versus warm-lookup cost of
    /// `plan_for` through the full configuration and credential path. Run with
    /// `cargo test -p qq --release -- --ignored plan_cache_cold_versus_warm --nocapture`.
    /// The root package has no library target, so this lives beside the unit
    /// tests instead of under `benches/`.
    #[test]
    #[ignore = "benchmark; run explicitly with --ignored --nocapture"]
    fn plan_cache_cold_versus_warm() {
        let fixture = RuntimeFixture::new();
        let credentials = CredentialStore::with_backend(
            CredentialPaths::new(fixture.path("data")),
            Arc::new(MemoryKeyring::default()),
        );
        credentials
            .set("openai/default", "sk-bench-secret", false)
            .unwrap();
        let factory = fixture.factory_with_credentials(credentials);
        fs::write(fixture.path("work/AGENTS.md"), "Be brief.\n".repeat(50)).unwrap();
        let request = fixture.request(r#"(version: 1, model: "openai/gpt-5.6")"#);
        let iterations: u32 = std::env::var("QQ_BENCH_ITERATIONS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(200);

        // Cold: force a miss each time by editing an instruction source. This
        // is the per-run cost every prompt paid before plans were cached.
        let mut cold_compile = Vec::with_capacity(iterations as usize);
        for index in 0..iterations {
            fs::write(
                fixture.path("work/AGENTS.md"),
                format!("Be brief. Iteration {index}.\n"),
            )
            .unwrap();
            let started = std::time::Instant::now();
            let (_, lookup) = factory
                .plan_with_lookup(&request, &AgentProfileId::default())
                .unwrap();
            cold_compile.push(started.elapsed());
            assert_eq!(lookup, PlanLookup::Compiled);
        }
        let mut warm = Vec::with_capacity(iterations as usize);
        for _ in 0..iterations {
            let started = std::time::Instant::now();
            let (_, lookup) = factory
                .plan_with_lookup(&request, &AgentProfileId::default())
                .unwrap();
            warm.push(started.elapsed());
            assert_eq!(lookup, PlanLookup::Hit);
        }
        let summarize = |mut samples: Vec<Duration>| {
            samples.sort();
            let index = |quantile: f64| {
                let position = ((samples.len() - 1) as f64 * quantile).round() as usize;
                samples[position]
            };
            (index(0.5), index(0.95))
        };
        let (cold_median, cold_p95) = summarize(cold_compile);
        let (warm_median, warm_p95) = summarize(warm);
        println!(
            "plan_for cold compile: median {:?} p95 {:?}; warm hit: median {:?} p95 {:?} \
             ({iterations} iterations)",
            cold_median, cold_p95, warm_median, warm_p95
        );
    }

    #[test]
    fn reviewer_verdict_parses_strictly_and_escalates_everything_else() {
        assert!(matches!(
            parse_reviewer_decision(r#"{"verdict":"approve"}"#),
            ReviewDecision::Approve
        ));
        assert!(matches!(
            parse_reviewer_decision("  {\"verdict\":\"approve\"}\n"),
            ReviewDecision::Approve
        ));
        assert!(matches!(
            parse_reviewer_decision(r#"{"verdict":"deny","reason":"wipes home"}"#),
            ReviewDecision::Deny { reason } if reason == "wipes home"
        ));
        assert!(matches!(
            parse_reviewer_decision(r#"{"verdict":"escalate","reason":"unsure"}"#),
            ReviewDecision::Escalate { reason } if reason == "unsure"
        ));
        // Unknown verdicts, prose-wrapped JSON, and non-JSON all escalate:
        // the reviewer can only ever expedite, never widen, an approval.
        assert!(matches!(
            parse_reviewer_decision(r#"{"verdict":"allow"}"#),
            ReviewDecision::Escalate { .. }
        ));
        assert!(matches!(
            parse_reviewer_decision(r#"Sure! {"verdict":"approve"}"#),
            ReviewDecision::Escalate { .. }
        ));
        assert!(matches!(
            parse_reviewer_decision("approve"),
            ReviewDecision::Escalate { .. }
        ));
        assert!(matches!(
            parse_reviewer_decision(""),
            ReviewDecision::Escalate { .. }
        ));
    }
}
