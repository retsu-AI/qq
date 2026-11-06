use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    path::{Path, PathBuf},
    sync::Arc,
};

use qq_auth::{AuthError, CredentialStore, Secret};
use qq_config::{
    AwsAuth, BedrockAuth, ClientSnapshot, ConfigError, ConfigLoader, ConfigSnapshot, EndpointMode,
    HttpAccess, HttpCredential, LoadRequest, ProviderAccess, ProviderApi, ProviderAuth,
    ProviderConfig, RuntimeOverrides,
};
use qq_core::{
    LoadedRuntime, PublishedEventStream, RuntimeConfigError, RuntimeLoadError, RuntimeLoadFuture,
    RuntimeLoadRequest, RuntimeLoader, SessionRuntime, SessionRuntimeError, SessionRuntimeOptions,
    SpawnModelValidationFuture, WorkerRuntimeLoadFuture,
    plan::{
        AgentProfile, CompiledAgentPlan, CredentialReference, HostSnapshot, PlanCompileError,
        ProviderDescriptor, SourceFingerprint,
    },
};
use qq_protocol::{
    AgentProfileId, AgentProfileSummary, ApprovalMode, CapabilitySupport, CommandRequest,
    GenerationCapabilities, ModelCatalogRequest, ModelDescriptor, ModelSelection,
    PromptCacheCapabilities, ProviderRequestShapeIdentity, ProviderRequestShapeVersion,
    ResolvedModel, ResolvedModelVersion, RunFailureKind, SnapshotRequest, SubscribeRequest,
    WorkspaceId,
};
use qq_provider::{
    BedrockAuth as ProviderBedrockAuth, CacheRouting, EndpointSpec, HttpAuth, HttpProtocol,
    HttpProviderRecipe, ProviderCompiler, ProviderError, ProviderRecipe, ResponsesBodyShape,
    SecretRef,
};
use qq_server::{
    CommandFuture, DelegationFuture, ModelsFuture, ProfilesFuture, ServerHandler,
    ServerHandlerError, ServerIdentity, SnapshotFuture, WorkspaceToolsFuture,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    HostBuild,
    builtin_auth::{self, BuiltinAuth},
    catalog::{DiscoveredModel, ModelDiscovery},
    plan::{
        CompiledGeneration, LiveBindings, PlanCache, PlanCacheError, PlanCacheLimits, PlanKey,
        PlanLookup,
    },
};

const MAX_MODEL_OPTIONS: usize = 4_096;
const MAX_DISCOVERY_PROVIDERS: usize = 4;

#[derive(Clone, Copy)]
struct CatalogSource<'a> {
    model: Option<&'a qq_config::ModelRoute>,
    organization: Option<&'a str>,
    max_output_tokens: Option<u32>,
    providers: &'a BTreeMap<String, ProviderConfig>,
    policy: &'a qq_config::EffectivePolicy,
}

impl<'a> From<&'a ConfigSnapshot> for CatalogSource<'a> {
    fn from(snapshot: &'a ConfigSnapshot) -> Self {
        Self {
            model: Some(snapshot.model()),
            organization: snapshot.organization(),
            max_output_tokens: chosen_max_output_tokens(
                snapshot.max_output_tokens(),
                snapshot.provenance(),
            ),
            providers: snapshot.providers(),
            policy: snapshot.policy(),
        }
    }
}

impl<'a> From<&'a ClientSnapshot> for CatalogSource<'a> {
    fn from(snapshot: &'a ClientSnapshot) -> Self {
        Self {
            model: snapshot.model(),
            organization: snapshot.organization(),
            max_output_tokens: chosen_max_output_tokens(
                snapshot.max_output_tokens(),
                snapshot.provenance(),
            ),
            providers: snapshot.providers(),
            policy: snapshot.policy(),
        }
    }
}

pub fn chosen_max_output_tokens(
    effective: u32,
    provenance: &qq_config::ConfigProvenance,
) -> Option<u32> {
    provenance
        .max_output_tokens()
        .is_some_and(|source| source.kind() != qq_config::SourceKind::Compiled)
        .then_some(effective)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderRemedy {
    pub provider: String,
    pub remedy: String,
}

#[derive(Clone, Debug)]
pub struct TuiModelState {
    pub configured_model: ModelSelection,
    pub model: Option<ModelSelection>,
    pub tui_model: ModelSelection,
    pub models: Vec<ModelDescriptor>,
    pub unauthenticated_providers: Vec<ProviderRemedy>,
    pub approval_timeout: Option<std::time::Duration>,
    pub retention: Option<std::time::Duration>,
    pub max_active_runs: usize,
}

#[derive(Clone)]
pub struct RuntimeFactory {
    inner: Arc<RuntimeFactoryInner>,
}

struct RuntimeFactoryInner {
    config: ConfigLoader,
    credentials: CredentialStore,
    mode: RuntimeMode,
    approved_read_images: qq_core::ApprovedReadImages,
    process_producer: Arc<qq_core::ProcessProducer>,
    providers: ProviderCompiler,
    discovery: ModelDiscovery,
    mcp: crate::mcp::McpRegistryCache,
    plans: PlanCache,
}

#[derive(Clone)]
enum RuntimeMode {
    Standard,
    IsolatedTuiQa { root: PathBuf, workspace: PathBuf },
}

fn validate_tui_qa_tree(path: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(path).map_err(|error| {
        format!(
            "could not inspect isolated fixture directory `{}`: {error}",
            path.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!(
                "could not inspect an entry in isolated fixture directory `{}`: {error}",
                path.display()
            )
        })?;
        let entry_path = entry.path();
        let metadata = std::fs::symlink_metadata(&entry_path).map_err(|error| {
            format!(
                "could not inspect isolated fixture entry `{}`: {error}",
                entry_path.display()
            )
        })?;
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "isolated fixture entry `{}` must not be a symbolic link",
                entry_path.display()
            ));
        }
        #[cfg(unix)]
        if metadata.is_file() {
            use std::os::unix::fs::MetadataExt as _;
            if metadata.nlink() != 1 {
                return Err(format!(
                    "isolated fixture entry `{}` must not be hard-linked",
                    entry_path.display()
                ));
            }
        }
        if metadata.is_dir() {
            validate_tui_qa_tree(&entry_path)?;
        }
    }
    Ok(())
}

impl RuntimeFactory {
    pub fn system_with_read_images(
        build: HostBuild,
        images: qq_core::ApprovedReadImages,
    ) -> Result<Self, RuntimeBuildError> {
        Self::new_with_read_images(
            build,
            ConfigLoader::system()?,
            CredentialStore::system()?,
            images,
        )
    }

    pub fn new_with_read_images(
        build: HostBuild,
        config: ConfigLoader,
        credentials: CredentialStore,
        images: qq_core::ApprovedReadImages,
    ) -> Result<Self, RuntimeBuildError> {
        Self::with_mode_and_images(build, config, credentials, RuntimeMode::Standard, images)
    }

    #[cfg(test)]
    pub fn new(
        config: ConfigLoader,
        credentials: CredentialStore,
    ) -> Result<Self, RuntimeBuildError> {
        Self::with_mode(
            tests::TEST_BUILD,
            config,
            credentials,
            RuntimeMode::Standard,
        )
    }

    pub fn isolated_tui_qa(
        build: HostBuild,
        config: ConfigLoader,
        credentials: CredentialStore,
        workspace: PathBuf,
    ) -> Result<Self, RuntimeBuildError> {
        let Some(root) = workspace.parent() else {
            return Err(RuntimeBuildError::InvalidTuiQaProfile {
                reason: "the isolated workspace has no fixture root".to_owned(),
            });
        };
        let paths = config.paths();
        if std::fs::canonicalize(&workspace).ok().as_ref() != Some(&workspace)
            || paths.global_dir() != root.join("config")
            || paths.data_dir() != root.join("data")
            || paths.managed_dir() != root.join("managed")
            || credentials.paths().data_dir() != root.join("credentials")
        {
            return Err(RuntimeBuildError::InvalidTuiQaProfile {
                reason: "configuration, credential, and workspace paths must share the isolated fixture root"
                    .to_owned(),
            });
        }
        Self::with_mode(
            build,
            config,
            credentials,
            RuntimeMode::IsolatedTuiQa {
                root: root.to_owned(),
                workspace,
            },
        )
    }

    fn with_mode(
        build: HostBuild,
        config: ConfigLoader,
        credentials: CredentialStore,
        mode: RuntimeMode,
    ) -> Result<Self, RuntimeBuildError> {
        Self::with_mode_and_images(
            build,
            config,
            credentials,
            mode,
            qq_core::ApprovedReadImages::default(),
        )
    }

    fn with_mode_and_images(
        build: HostBuild,
        config: ConfigLoader,
        credentials: CredentialStore,
        mode: RuntimeMode,
        approved_read_images: qq_core::ApprovedReadImages,
    ) -> Result<Self, RuntimeBuildError> {
        Ok(Self {
            inner: Arc::new(RuntimeFactoryInner {
                config,
                credentials,
                mode,
                approved_read_images,
                process_producer: Arc::new(qq_core::ProcessProducer::new(build.source_revision)),
                providers: ProviderCompiler::new()?,
                discovery: ModelDiscovery::new()?,
                mcp: crate::mcp::McpRegistryCache::new(),
                plans: PlanCache::new(PlanCacheLimits::default()),
            }),
        })
    }

    pub fn tui_model_state(
        &self,
        snapshot: &ClientSnapshot,
        request: &LoadRequest,
    ) -> TuiModelState {
        let models = self.client_model_options(snapshot);
        let unauthenticated_providers = self.unauthenticated_providers(snapshot);
        let configured_model = snapshot
            .model()
            .map_or_else(ModelSelection::default, |route| ModelSelection {
                model_is_fallback: request.overrides().model().is_none(),
                model: Some(route.as_str().to_owned()),
                max_output_tokens: chosen_max_output_tokens(
                    snapshot.max_output_tokens(),
                    snapshot.provenance(),
                ),
                organization: snapshot.organization().map(str::to_owned),
            });
        let model = (configured_model.model.is_some()
            && models
                .iter()
                .any(|option| option.selection.model == configured_model.model))
        .then(|| configured_model.clone());
        let configured_provider_unauthenticated = configured_model
            .model
            .as_deref()
            .and_then(|route| route.split_once('/'))
            .is_some_and(|(provider, _)| {
                unauthenticated_providers
                    .iter()
                    .any(|remedy| remedy.provider == provider)
            });
        let tui_model = match &model {
            Some(model) => model.clone(),
            None if configured_provider_unauthenticated => configured_model.clone(),
            None => ModelSelection::default(),
        };
        TuiModelState {
            configured_model,
            model,
            tui_model,
            models,
            unauthenticated_providers,
            approval_timeout: snapshot.approval_timeout(),
            retention: snapshot.retention(),
            max_active_runs: snapshot.max_active_runs(),
        }
    }

    pub fn load(&self, request: &LoadRequest) -> Result<ConfigSnapshot, RuntimeBuildError> {
        self.validate_isolated_tui_qa_state()?;
        self.validate_tui_qa_workspace(request.cwd())?;
        let snapshot = self.inner.config.load(request)?;
        if self.is_isolated_tui_qa() {
            self.validate_tui_qa_snapshot(&snapshot)?;
        }
        Ok(snapshot)
    }

    pub fn load_for_client(
        &self,
        request: &LoadRequest,
    ) -> Result<ClientSnapshot, RuntimeBuildError> {
        self.validate_isolated_tui_qa_state()?;
        self.validate_tui_qa_workspace(request.cwd())?;
        if self.is_isolated_tui_qa() {
            let snapshot = self.inner.config.load(request)?;
            self.validate_tui_qa_snapshot(&snapshot)?;
        }
        Ok(self.inner.config.load_for_client(request)?)
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn configured_model_options(&self, snapshot: &ConfigSnapshot) -> Vec<ModelDescriptor> {
        if self.is_isolated_tui_qa() {
            vec![Self::isolated_tui_qa_model_option(snapshot)]
        } else {
            self.model_options_with_discovery(
                &CatalogSource::from(snapshot),
                &self.cached_model_discovery(snapshot.providers()),
            )
        }
    }

    pub fn client_model_options(&self, snapshot: &ClientSnapshot) -> Vec<ModelDescriptor> {
        if self.is_isolated_tui_qa() {
            return snapshot
                .model()
                .map(|route| vec![Self::isolated_tui_qa_model_option_for(snapshot, route)])
                .unwrap_or_default();
        }
        self.model_options_with_discovery(
            &CatalogSource::from(snapshot),
            &self.cached_model_discovery(snapshot.providers()),
        )
    }

    pub fn unauthenticated_providers(&self, snapshot: &ClientSnapshot) -> Vec<ProviderRemedy> {
        if self.is_isolated_tui_qa() {
            return Vec::new();
        }
        let allowed = snapshot.policy().allowed_providers();
        let denied = snapshot.policy().denied_providers();
        let mut remedies = Vec::new();
        for (provider_id, provider) in snapshot.providers() {
            if allowed.is_some_and(|allowed| !allowed.iter().any(|id| id == provider_id))
                || denied.iter().any(|id| id == provider_id)
            {
                continue;
            }
            let remedy = match provider.access() {
                Some(ProviderAccess::Http(access)) => match access.auth() {
                    HttpCredential::Builtin {
                        provider,
                        api_key: None,
                        ..
                    } => match provider.environment_variable() {
                        Some(variable) => {
                            format!("run qq auth login {provider_id} or set {variable}")
                        }
                        None => format!("run qq auth login {provider_id}"),
                    },
                    HttpCredential::Builtin {
                        api_key: Some(_), ..
                    }
                    | HttpCredential::Configured(_) => continue,
                },
                Some(
                    ProviderAccess::AmazonBedrock { auth, .. }
                    | ProviderAccess::AmazonBedrockMantle { auth, .. },
                ) => match auth {
                    BedrockAuth::Aws(AwsAuth::DefaultChain) => {
                        "set AWS_PROFILE or AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY".to_owned()
                    }
                    BedrockAuth::Aws(AwsAuth::Profile(profile)) => {
                        format!("configure the AWS profile {profile}")
                    }
                    BedrockAuth::ApiKey(_) => continue,
                },
                None => continue,
            };
            if self.provider_authenticated(provider_id, provider) {
                continue;
            }
            remedies.push(ProviderRemedy {
                provider: provider_id.clone(),
                remedy,
            });
        }
        remedies
    }

    pub fn isolated_tui_qa_model_option(snapshot: &ConfigSnapshot) -> ModelDescriptor {
        Self::isolated_tui_qa_model_option_for(CatalogSource::from(snapshot), snapshot.model())
    }

    fn isolated_tui_qa_model_option_for<'a>(
        snapshot: impl Into<CatalogSource<'a>>,
        route: &qq_config::ModelRoute,
    ) -> ModelDescriptor {
        let snapshot = snapshot.into();
        let metadata = snapshot
            .providers
            .get(route.provider())
            .and_then(|provider| provider.models().get(route.model()));
        ModelDescriptor {
            provider: route.provider().to_owned(),
            model: route.model().to_owned(),
            name: metadata
                .and_then(|metadata| metadata.name())
                .map(str::to_owned),
            context_window: snapshot
                .providers
                .get(route.provider())
                .and_then(|provider| provider.model_context_window(route.model())),
            reasoning_efforts: metadata
                .map(|metadata| metadata.reasoning_efforts().to_vec())
                .unwrap_or_default(),
            selection: ModelSelection {
                model_is_fallback: false,
                model: Some(route.as_str().to_owned()),
                max_output_tokens: snapshot.max_output_tokens.map(|chosen| {
                    metadata
                        .and_then(|metadata| metadata.max_output_tokens())
                        .map_or(chosen, |limit| limit.min(chosen))
                }),
                organization: snapshot.organization.map(str::to_owned),
            },
        }
    }

    fn model_options_with_discovery(
        &self,
        snapshot: &CatalogSource<'_>,
        discovered: &BTreeMap<String, Vec<DiscoveredModel>>,
    ) -> Vec<ModelDescriptor> {
        self.model_options_with_discovery_impl(snapshot, discovered, RouteAdmission::Authenticated)
    }

    pub(super) fn model_options_for_plan(&self, snapshot: &ConfigSnapshot) -> Vec<ModelDescriptor> {
        if self.is_isolated_tui_qa() {
            return vec![Self::isolated_tui_qa_model_option(snapshot)];
        }
        let registered: BTreeSet<String> = match self.inner.credentials.list() {
            Ok(records) => records.into_iter().map(|record| record.name).collect(),
            Err(_) => BTreeSet::new(),
        };
        let discovered = self.cached_model_discovery(snapshot.providers());
        self.model_options_with_discovery_impl(
            &CatalogSource::from(snapshot),
            &discovered,
            RouteAdmission::CredentialPresent(&registered),
        )
    }

    fn cached_model_discovery(
        &self,
        providers: &BTreeMap<String, ProviderConfig>,
    ) -> BTreeMap<String, Vec<DiscoveredModel>> {
        providers
            .iter()
            .filter_map(|(id, provider)| {
                self.inner
                    .discovery
                    .cached(id, provider, &self.inner.credentials)
                    .map(|models| (id.clone(), models))
            })
            .collect()
    }

    fn model_options_with_discovery_impl(
        &self,
        snapshot: &CatalogSource<'_>,
        discovered: &BTreeMap<String, Vec<DiscoveredModel>>,
        admission: RouteAdmission<'_>,
    ) -> Vec<ModelDescriptor> {
        let admitted = |provider_id: &str, provider: &ProviderConfig| match admission {
            RouteAdmission::Authenticated => self.provider_authenticated(provider_id, provider),
            RouteAdmission::CredentialPresent(registered) => {
                provider_credential_present(provider, registered)
            }
        };
        let allowed = snapshot.policy.allowed_providers();
        let denied = snapshot.policy.denied_providers();
        let mut options = Vec::new();
        'providers: for (provider_id, provider) in snapshot.providers {
            if allowed.is_some_and(|allowed| !allowed.iter().any(|id| id == provider_id))
                || denied.iter().any(|id| id == provider_id)
                || !admitted(provider_id, provider)
            {
                continue;
            }
            for (model_id, metadata) in provider.models() {
                if !model_in_catalog(
                    provider,
                    model_id,
                    discovered.get(provider_id).map(Vec::as_slice),
                ) {
                    continue;
                }
                if options.len() >= MAX_MODEL_OPTIONS {
                    break 'providers;
                }
                options.push(ModelDescriptor {
                    provider: provider_id.clone(),
                    model: model_id.clone(),
                    name: metadata.name().map(str::to_owned),
                    context_window: provider.model_context_window(model_id),
                    reasoning_efforts: discovered
                        .get(provider_id)
                        .and_then(|models| models.iter().find(|model| &model.id == model_id))
                        .and_then(|model| model.efforts.clone())
                        .filter(|_| !metadata.explicitly_configured())
                        .unwrap_or_else(|| metadata.reasoning_efforts().to_vec()),
                    selection: qq_protocol::ModelSelection {
                        model_is_fallback: false,
                        model: Some(format!("{provider_id}/{model_id}")),
                        max_output_tokens: snapshot.max_output_tokens.map(|chosen| {
                            metadata
                                .max_output_tokens()
                                .map_or(chosen, |limit| limit.min(chosen))
                        }),
                        organization: snapshot.organization.map(str::to_owned),
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
                        context_window: provider.model_context_window(&model.id),
                        reasoning_efforts: model.efforts.clone().unwrap_or_default(),
                        selection: qq_protocol::ModelSelection {
                            model_is_fallback: false,
                            model: Some(format!("{provider_id}/{}", model.id)),
                            max_output_tokens: snapshot.max_output_tokens,
                            organization: snapshot.organization.map(str::to_owned),
                        },
                    });
                }
            }
        }
        if let Some(route) = snapshot.model
            && options.len() < MAX_MODEL_OPTIONS
            && !options
                .iter()
                .any(|option| option.selection.model.as_deref() == Some(route.as_str()))
            && allowed.is_none_or(|allowed| allowed.iter().any(|id| id == route.provider()))
            && !denied.iter().any(|id| id == route.provider())
            && let Some(provider) = snapshot.providers.get(route.provider())
            && admitted(route.provider(), provider)
        {
            let metadata = provider.models().get(route.model());
            options.push(ModelDescriptor {
                provider: route.provider().to_owned(),
                model: route.model().to_owned(),
                name: None,
                context_window: provider.model_context_window(route.model()),
                reasoning_efforts: metadata
                    .map(|metadata| metadata.reasoning_efforts().to_vec())
                    .unwrap_or_default(),
                selection: qq_protocol::ModelSelection {
                    model_is_fallback: false,
                    model: Some(route.as_str().to_owned()),
                    max_output_tokens: snapshot.max_output_tokens,
                    organization: snapshot.organization.map(str::to_owned),
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

    fn discovered_model_options(&self, snapshot: &ClientSnapshot) -> Vec<ModelDescriptor> {
        if self.is_isolated_tui_qa() {
            return snapshot
                .model()
                .map(|route| vec![Self::isolated_tui_qa_model_option_for(snapshot, route)])
                .unwrap_or_default();
        }
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
        self.model_options_with_discovery(&CatalogSource::from(snapshot), &discovered)
    }

    fn is_isolated_tui_qa(&self) -> bool {
        matches!(self.inner.mode, RuntimeMode::IsolatedTuiQa { .. })
    }

    fn validate_tui_qa_workspace(&self, workspace: &Path) -> Result<(), RuntimeBuildError> {
        let RuntimeMode::IsolatedTuiQa {
            workspace: isolated,
            ..
        } = &self.inner.mode
        else {
            return Ok(());
        };
        if workspace != isolated {
            return Err(RuntimeBuildError::InvalidTuiQaProfile {
                reason: format!(
                    "workspace `{}` is outside the isolated fixture",
                    workspace.display()
                ),
            });
        }
        Ok(())
    }

    pub fn validate_isolated_tui_qa_state(&self) -> Result<(), RuntimeBuildError> {
        let RuntimeMode::IsolatedTuiQa { root, workspace } = &self.inner.mode else {
            return Ok(());
        };
        let invalid = |reason: String| RuntimeBuildError::InvalidTuiQaProfile { reason };
        let root_metadata = std::fs::symlink_metadata(root).map_err(|error| {
            invalid(format!(
                "could not revalidate isolated fixture root `{}`: {error}",
                root.display()
            ))
        })?;
        if !root_metadata.is_dir()
            || root_metadata.file_type().is_symlink()
            || std::fs::canonicalize(root).ok().as_ref() != Some(root)
        {
            return Err(invalid(
                "the isolated fixture root was replaced after startup".to_owned(),
            ));
        }
        let required_directories = [
            root.join("config"),
            root.join("data"),
            root.join("credentials"),
            root.join("managed"),
            root.join("runtime"),
            workspace.clone(),
        ];
        for path in required_directories {
            let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                invalid(format!(
                    "could not revalidate isolated fixture directory `{}`: {error}",
                    path.display()
                ))
            })?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || std::fs::canonicalize(&path).ok().as_ref() != Some(&path)
            {
                return Err(invalid(format!(
                    "isolated fixture directory `{}` was replaced after startup",
                    path.display()
                )));
            }
        }
        if workspace.parent() != Some(root.as_path()) {
            return Err(invalid(
                "the isolated workspace no longer belongs to its fixture root".to_owned(),
            ));
        }
        validate_tui_qa_tree(root).map_err(invalid)
    }

    fn request_for_workspace(
        &self,
        workspace: &Path,
        max_output_tokens: Option<u32>,
    ) -> Result<LoadRequest, RuntimeBuildError> {
        self.validate_isolated_tui_qa_state()?;
        self.validate_tui_qa_workspace(workspace)?;
        let max_output_tokens =
            max_output_tokens.filter(|limit| !LEGACY_DEFAULT_MAX_OUTPUT_TOKENS.contains(limit));
        let request = if self.is_isolated_tui_qa() {
            let mut overrides = RuntimeOverrides::new();
            if let Some(max_output_tokens) = max_output_tokens {
                overrides = overrides.with_max_output_tokens(max_output_tokens);
            }
            LoadRequest::new(workspace).with_overrides(overrides)
        } else {
            LoadRequest::from_process_env(workspace, max_output_tokens)?
        };
        Ok(request.with_untrusted_project_config_ignored())
    }

    fn validate_tui_qa_snapshot(&self, snapshot: &ConfigSnapshot) -> Result<(), RuntimeBuildError> {
        let invalid = |reason: &str| RuntimeBuildError::InvalidTuiQaProfile {
            reason: reason.to_owned(),
        };
        if !self.inner.credentials.list()?.is_empty() {
            return Err(invalid(
                "stored credentials are not allowed in an isolated TUI QA fixture",
            ));
        }
        let provider = snapshot
            .providers()
            .get(snapshot.model().provider())
            .ok_or_else(|| invalid("the selected provider is unavailable"))?;
        if provider.kind() != qq_config::ProviderKind::Custom {
            return Err(invalid("the selected provider must be Custom"));
        }
        if !provider.models().contains_key(snapshot.model().model()) {
            return Err(invalid(
                "the selected model must be declared by the provider",
            ));
        }
        let Some(ProviderAccess::Http(access)) = provider.access() else {
            return Err(invalid("the selected provider must use HTTP"));
        };
        if !matches!(
            access.auth(),
            HttpCredential::Configured(ProviderAuth::NoAuth)
        ) || !access.headers().is_empty()
        {
            return Err(invalid(
                "the selected provider must use NoAuth with no static headers",
            ));
        }
        let endpoint = reqwest::Url::parse(access.endpoint())
            .map_err(|_| invalid("the selected provider endpoint is invalid"))?;
        let loopback = endpoint.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        });
        if endpoint.scheme() != "http"
            || !loopback
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return Err(invalid(
                "the selected provider endpoint must be credential-free loopback HTTP",
            ));
        }
        if snapshot.organization().is_some()
            || !snapshot.mcp_servers().is_empty()
            || snapshot.worker_model().is_some()
            || !snapshot.delegation().roster().is_empty()
            || snapshot.audit().mode() != qq_config::AuditMode::Off
            || !snapshot.profiles().is_empty()
        {
            return Err(invalid(
                "organizations, MCP, worker routes, delegation, audit, and profiles are not allowed",
            ));
        }
        Ok(())
    }

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
            approval_mode: ApprovalMode::default(),
        });
        for (name, profile) in snapshot.profiles() {
            let id = match AgentProfileId::new(name.clone()) {
                Ok(id) => id,
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

    pub fn delegation_for(
        &self,
        workspace: &str,
    ) -> Result<qq_protocol::DelegationRoster, RuntimeBuildError> {
        let snapshot = self.snapshot_for_selection(workspace, &ModelSelection::default())?;
        Ok(delegation_roster(&snapshot, snapshot.model(), |route| {
            self.discovered_efforts(&snapshot, route)
        }))
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
            self.request_for_workspace(&workspace, request.selection.max_output_tokens)?;
        let mut overrides = load.overrides().clone();
        if let Some(organization) = &request.selection.organization {
            overrides = overrides.with_organization(organization.clone());
        }
        load = load.with_overrides(overrides);
        let snapshot = self.load_for_client(&load)?;
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
                HttpCredential::Builtin {
                    provider,
                    api_key,
                    profile,
                } => BuiltinAuth::of(provider)
                    .check(
                        &self.inner.credentials,
                        provider,
                        api_key.as_ref(),
                        profile.as_deref(),
                    )
                    .is_ok(),
            },
            Some(
                ProviderAccess::AmazonBedrock { auth, .. }
                | ProviderAccess::AmazonBedrockMantle { auth, .. },
            ) => match auth {
                BedrockAuth::ApiKey(reference) => self.inner.credentials.resolve(reference).is_ok(),
                BedrockAuth::Aws(AwsAuth::Profile(profile)) => aws_profile_configured(profile),
                BedrockAuth::Aws(AwsAuth::DefaultChain) => aws_default_chain_source().is_some(),
            },
            None => false,
        }
    }

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
        let mut load = self.request_for_workspace(&workspace, selection.max_output_tokens)?;
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

    fn validate_spawn_snapshot(
        &self,
        snapshot: &ConfigSnapshot,
        workspace: &str,
        advertised: &[String],
    ) -> Result<(), RuntimeBuildError> {
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
        if advertised
            .iter()
            .any(|route| route == snapshot.model().as_str())
        {
            return Ok(());
        }
        let cached = self
            .inner
            .discovery
            .cached(provider_id, provider, &self.inner.credentials);
        if model_in_catalog(provider, model_id, cached.as_deref()) {
            return Ok(());
        }
        let configured =
            self.load_for_client(&self.request_for_workspace(Path::new(workspace), None)?)?;
        if configured.model() == Some(snapshot.model()) {
            return Ok(());
        }
        let discovered = cached.or_else(|| {
            self.inner
                .discovery
                .discover(provider_id, provider, &self.inner.credentials)
        });
        if model_in_catalog(provider, model_id, discovered.as_deref()) {
            return Ok(());
        }
        Err(RuntimeBuildError::UnknownModel {
            provider: provider_id.to_owned(),
            model: model_id.to_owned(),
        })
    }

    fn validate_spawn_selection(
        &self,
        workspace: &str,
        selection: &qq_protocol::ModelSelection,
        advertised: &[String],
    ) -> Result<(), RuntimeLoadError> {
        let snapshot = self
            .snapshot_for_selection(workspace, selection)
            .map_err(|error| RuntimeLoadError {
                kind: error.failure_kind(),
                message: error.to_string(),
            })?;
        self.validate_spawn_snapshot(&snapshot, workspace, advertised)
            .map_err(|error| {
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

    fn served_route_hint(&self, snapshot: &ConfigSnapshot) -> Option<String> {
        const MAX_LISTED_ROUTES: usize = 12;
        let provider_id = snapshot.model().provider();
        let provider = snapshot.providers().get(provider_id)?;
        let mut ids: BTreeSet<String> = provider.models().keys().cloned().collect();
        if let Some(discovered) =
            self.inner
                .discovery
                .cached(provider_id, provider, &self.inner.credentials)
        {
            ids.retain(|id| model_in_catalog(provider, id, Some(&discovered)));
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

    pub fn plan_for(
        &self,
        request: &LoadRequest,
    ) -> Result<Arc<CompiledAgentPlan>, RuntimeBuildError> {
        self.plan_with_lookup(request, &AgentProfileId::default())
            .map(|(plan, _)| plan)
    }

    #[cfg(test)]
    fn plan_for_profile(
        &self,
        request: &LoadRequest,
        profile: &AgentProfileId,
    ) -> Result<Arc<CompiledAgentPlan>, RuntimeBuildError> {
        self.plan_with_lookup_progress(request, profile, None)
            .map(|(plan, _)| plan)
    }

    #[cfg(test)]
    fn plan_for_profile_with_progress(
        &self,
        request: &LoadRequest,
        profile: &AgentProfileId,
        progress: &qq_core::RuntimeLoadProgress,
    ) -> Result<Arc<CompiledAgentPlan>, RuntimeBuildError> {
        self.plan_with_lookup_progress(request, profile, Some(progress))
            .map(|(plan, _)| plan)
    }

    pub fn plan_with_lookup(
        &self,
        request: &LoadRequest,
        profile: &AgentProfileId,
    ) -> Result<(Arc<CompiledAgentPlan>, PlanLookup), RuntimeBuildError> {
        self.plan_with_lookup_progress(request, profile, None)
    }

    fn plan_with_lookup_progress(
        &self,
        request: &LoadRequest,
        profile: &AgentProfileId,
        progress: Option<&qq_core::RuntimeLoadProgress>,
    ) -> Result<(Arc<CompiledAgentPlan>, PlanLookup), RuntimeBuildError> {
        self.plan_in_execution_root(request, profile, progress, None)
    }

    fn plan_in_execution_root(
        &self,
        request: &LoadRequest,
        profile: &AgentProfileId,
        progress: Option<&qq_core::RuntimeLoadProgress>,
        execution_root: Option<&Path>,
    ) -> Result<(Arc<CompiledAgentPlan>, PlanLookup), RuntimeBuildError> {
        preparation_phase(progress, qq_core::RuntimeLoadStage::CanonicalizingWorkspace)?;
        self.validate_isolated_tui_qa_state()?;
        self.validate_tui_qa_workspace(request.cwd())?;
        let configuration_workspace = qq_config::canonical_working_directory(request.cwd())?;
        let workspace = match execution_root {
            Some(root) => qq_config::canonical_working_directory(root)?,
            None => configuration_workspace.clone(),
        };
        let key = PlanKey {
            configuration_workspace,
            workspace: workspace.clone(),
            model: ModelSelection {
                model_is_fallback: false,
                model: request.overrides().model().map(str::to_owned),
                max_output_tokens: request.overrides().max_output_tokens(),
                organization: request.overrides().organization().map(str::to_owned),
            },
            profile: profile.clone(),
            explicit_config_path: request.explicit_path().map(Path::to_owned),
            explicit_config_content: request.explicit_content().map(str::to_owned),
            reasoning_effort: request.overrides().reasoning_effort(),
            ignore_untrusted_project_config: request.ignores_untrusted_project_config(),
        };
        let lookup =
            self.inner
                .plans
                .load_with_discovery_epoch(&key, self.inner.discovery.epoch(), || {
                    self.compile_generation(request, profile, &workspace, progress)
                });
        match lookup {
            Ok((plan, lookup)) => Ok((plan, lookup)),
            Err(PlanCacheError::Compile(error)) => Err(error),
            Err(PlanCacheError::Capacity { .. }) => Err(RuntimeBuildError::PlanCacheFull),
            Err(PlanCacheError::ShutDown) => Err(RuntimeBuildError::PlanCacheShutDown),
            Err(PlanCacheError::Poisoned) => Err(RuntimeBuildError::CacheUnavailable),
        }
    }

    pub fn shutdown_plans(&self) {
        self.inner.plans.shutdown();
    }

    fn compile_generation(
        &self,
        request: &LoadRequest,
        profile_id: &AgentProfileId,
        workspace: &Path,
        progress: Option<&qq_core::RuntimeLoadProgress>,
    ) -> Result<CompiledGeneration, RuntimeBuildError> {
        preparation_phase(
            progress,
            qq_core::RuntimeLoadStage::ReadingCredentialMetadata,
        )?;
        let credential_index =
            SourceFingerprint::capture(self.inner.credentials.paths().index_file());
        let epoch = self.inner.credentials.epoch()?;
        preparation_phase(progress, qq_core::RuntimeLoadStage::LoadingConfiguration)?;
        let snapshot = self.load(request)?;
        let mut configuration_sources = vec![snapshot.sources().clone()];
        let selected_profile = snapshot.profile(profile_id.as_str());
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
                if request.overrides().reasoning_effort().is_none()
                    && let Some(effort) = profile.reasoning_effort()
                {
                    overrides = overrides.with_reasoning_effort(effort);
                }
                preparation_phase(progress, qq_core::RuntimeLoadStage::LoadingConfiguration)?;
                let snapshot = self.load(&request.clone().with_overrides(overrides))?;
                if !configuration_sources.contains(snapshot.sources()) {
                    configuration_sources.push(snapshot.sources().clone());
                }
                snapshot
            }
        };
        let live_efforts = self.discovered_efforts(&snapshot, snapshot.model());
        let snapshot = with_claude_default_effort(snapshot, live_efforts.as_deref());
        preparation_phase(progress, qq_core::RuntimeLoadStage::CompilingProvider)?;
        let resolved_model = self.resolved_model_for_snapshot(&snapshot)?;
        let output_ceiling = snapshot
            .providers()
            .get(snapshot.model().provider())
            .and_then(|provider| provider.models().get(snapshot.model().model()))
            .and_then(qq_config::ModelMetadata::max_output_tokens)
            .map(|limit| match snapshot.policy().max_output_tokens() {
                Some(ceiling) if ceiling < limit => qq_core::OutputCeiling {
                    tokens: ceiling,
                    policy_bound: true,
                },
                _ => qq_core::OutputCeiling {
                    tokens: limit,
                    policy_bound: false,
                },
            });
        if snapshot
            .reasoning_effort()
            .is_some_and(|effort| effort != qq_provider::ReasoningEffort::Default)
            && resolved_model.generation.reasoning_effort
                == qq_protocol::CapabilitySupport::Unsupported
        {
            return Err(RuntimeBuildError::UnsupportedReasoningEffort(
                snapshot.model().as_str().to_owned(),
            ));
        }
        if let Some(effort) = snapshot
            .reasoning_effort()
            .filter(|effort| *effort != qq_provider::ReasoningEffort::Default)
            && snapshot
                .providers()
                .get(snapshot.model().provider())
                .is_some_and(|provider| {
                    provider.access().is_some_and(|access| {
                        effective_provider_api(provider, snapshot.model().model(), access)
                            == ProviderApi::AnthropicMessages
                    })
                })
            && matches!(
                effort,
                qq_provider::ReasoningEffort::None | qq_provider::ReasoningEffort::Minimal
            )
        {
            return Err(RuntimeBuildError::UnsupportedReasoningEffort(
                snapshot.model().as_str().to_owned(),
            ));
        }
        if let Some(effort) = snapshot
            .reasoning_effort()
            .filter(|effort| *effort != qq_provider::ReasoningEffort::Default)
            && let Some(levels) = &live_efforts
            && !levels.contains(&effort)
        {
            return Err(RuntimeBuildError::ReasoningEffortNotAdvertised {
                model: snapshot.model().as_str().to_owned(),
                effort,
                advertised: levels.clone(),
            });
        }
        if let Some(effort) = snapshot
            .reasoning_effort()
            .filter(|effort| *effort != qq_provider::ReasoningEffort::Default)
            && live_efforts.is_none()
            && let Some(metadata) = snapshot
                .providers()
                .get(snapshot.model().provider())
                .and_then(|provider| provider.models().get(snapshot.model().model()))
            && !metadata.reasoning_efforts().is_empty()
            && !metadata.reasoning_efforts().contains(&effort)
        {
            return Err(RuntimeBuildError::ReasoningEffortNotAdvertised {
                model: snapshot.model().as_str().to_owned(),
                effort,
                advertised: metadata.reasoning_efforts().to_vec(),
            });
        }

        let provider_id = snapshot.model().provider();
        let provider_config = snapshot
            .providers()
            .get(provider_id)
            .ok_or_else(|| RuntimeBuildError::UnknownProvider(provider_id.to_owned()))?;
        preparation_phase(progress, qq_core::RuntimeLoadStage::CompilingProvider)?;
        let (recipe, descriptor) =
            self.prepare_provider(provider_id, snapshot.model().model(), provider_config)?;
        preparation_phase(progress, qq_core::RuntimeLoadStage::CompilingProvider)?;
        let provider = self.inner.providers.compile(recipe)?;
        let spawn_model_routes = self
            .model_options_for_plan(&snapshot)
            .into_iter()
            .filter_map(|model| model.selection.model)
            .collect();
        let provenance = snapshot
            .source_reports()
            .iter()
            .map(|report| report.source().label().to_owned())
            .collect();
        let delegation = delegation_roster(&snapshot, snapshot.model(), |route| {
            self.discovered_efforts(&snapshot, route)
        });
        let audit = audit_policy(snapshot.audit());
        let shell = shell_policy(snapshot.policy(), snapshot.shell());
        let network = network_policy(snapshot.policy());
        let mut profile =
            AgentProfile::new(provider, descriptor, resolved_model, workspace.to_owned())
                .with_spawn_model_routes(spawn_model_routes)
                .with_delegation(delegation)
                .with_audit(audit)
                .with_shell_policy(shell)
                .with_approved_read_images(self.inner.approved_read_images.clone())
                .with_process_producer(Arc::clone(&self.inner.process_producer))
                .with_network_policy(network)
                .with_output_ceiling(output_ceiling)
                .with_provenance(provenance)
                .with_configuration_workspace(request.cwd().to_owned())
                .with_credential_epoch(epoch)
                .with_profile_id(profile_id.clone())
                .with_global_config_dir(self.inner.config.paths().global_dir().to_owned());
        if let Some(effort) = snapshot.reasoning_effort() {
            profile = profile.with_reasoning_effort(effort);
        }
        if let Some(names) = snapshot.policy().exposed_tools() {
            profile = profile.with_exposed_tools(names.to_vec());
        }
        profile = profile.with_side_question_denies(snapshot.policy().deny_tools().to_vec());
        let mut bindings = LiveBindings {
            provider: provider_config.access().cloned(),
            mcp: None,
            runtime: crate::plan::RuntimeSwitches { output_ceiling },
        };
        preparation_phase(progress, qq_core::RuntimeLoadStage::LoadingTools)?;
        if let Some(wired) =
            self.inner
                .mcp
                .registry_for_snapshot(&self.inner.credentials, epoch, &snapshot)?
        {
            bindings.mcp = Some(Arc::clone(&wired.registry));
            profile = profile
                .with_host(HostSnapshot::capture_blocking(wired.registry))
                .with_mcp_servers(wired.servers);
        }
        preparation_phase(progress, qq_core::RuntimeLoadStage::CompilingPlan)?;
        let plan = CompiledAgentPlan::compile_blocking(profile)?;
        let mut sources = Vec::with_capacity(plan.instruction_sources().len() + 1);
        sources.push(credential_index);
        sources.extend(plan.instruction_sources().iter().cloned());
        preparation_phase(progress, qq_core::RuntimeLoadStage::Complete)?;
        Ok(CompiledGeneration {
            plan,
            sources,
            configuration_sources,
            bindings,
        })
    }

    fn discovered_efforts(
        &self,
        snapshot: &ConfigSnapshot,
        route: &qq_config::ModelRoute,
    ) -> Option<Vec<qq_provider::ReasoningEffort>> {
        let provider = snapshot.providers().get(route.provider())?;
        if provider
            .models()
            .get(route.model())
            .is_some_and(|metadata| metadata.explicitly_configured())
        {
            return None;
        }
        self.inner
            .discovery
            .cached(route.provider(), provider, &self.inner.credentials)?
            .into_iter()
            .find(|model| model.id == route.model())
            .and_then(|model| model.efforts)
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
        let api = effective_provider_api(provider, snapshot.model().model(), access);
        let model_limit = metadata.and_then(qq_config::ModelMetadata::max_output_tokens);
        let configured_explicitly = snapshot
            .provenance()
            .max_output_tokens()
            .is_some_and(|source| source.kind() != qq_config::SourceKind::Compiled);
        let effort_shares_output_cap = snapshot.reasoning_effort().is_some_and(|effort| {
            !matches!(
                effort,
                qq_provider::ReasoningEffort::Default | qq_provider::ReasoningEffort::None
            )
        }) && (api == ProviderApi::AnthropicMessages
            || anthropic_model_behind_a_gateway(
                snapshot.model().model(),
                metadata.and_then(qq_config::ModelMetadata::canonical_id),
            ));
        let max_output_tokens = match (
            model_limit,
            effort_shares_output_cap && !configured_explicitly,
        ) {
            (Some(model_limit), true) => snapshot
                .policy()
                .max_output_tokens()
                .map_or(model_limit, |ceiling| model_limit.min(ceiling)),
            (Some(model_limit), false) => model_limit.min(snapshot.max_output_tokens()),
            (None, _) => snapshot.max_output_tokens(),
        };
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
            ProviderAccess::Http(access) if responses_body_shape(access) == ResponsesBodyShape::Codex
        );
        let (cache_read_usage, cache_write_usage, cache_control) = match api {
            ProviderApi::OpenAiResponses
            | ProviderApi::OpenAiChatCompletions
            | ProviderApi::GoogleGenerateContent => (true, false, CapabilitySupport::Unsupported),
            ProviderApi::AnthropicMessages | ProviderApi::BedrockConverse => {
                (true, true, CapabilitySupport::Native)
            }
        };
        let credential_profile = match access {
            ProviderAccess::Http(access) => match access.auth() {
                HttpCredential::Builtin {
                    provider, profile, ..
                } => BuiltinAuth::of(provider)
                    .is_profile_scoped()
                    .then(|| builtin_auth::profile_name(profile.as_deref()).to_owned()),
                HttpCredential::Configured(_) => None,
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
            context_window: provider.model_context_window(snapshot.model().model()),
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
                reasoning_effort: if matches!(access, ProviderAccess::Http(_))
                    && matches!(
                        api,
                        ProviderApi::OpenAiResponses
                            | ProviderApi::OpenAiChatCompletions
                            | ProviderApi::AnthropicMessages
                    ) {
                    CapabilitySupport::Native
                } else {
                    CapabilitySupport::Unsupported
                },
            },
            prompt_cache: PromptCacheCapabilities {
                control: cache_control,
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
            HttpCredential::Builtin {
                provider,
                api_key,
                profile,
            } => match BuiltinAuth::of(provider) {
                BuiltinAuth::ApiKey { .. } => {
                    let secret = builtin_auth::resolve_api_key(
                        &self.inner.credentials,
                        provider,
                        api_key.as_ref(),
                        profile.as_deref(),
                    )?;
                    let credential = match api_key {
                        Some(reference) => credential_reference(reference),
                        None => {
                            let stored_name = provider.stored_name(profile.as_deref());
                            if self.inner.credentials.is_registered(&stored_name)? {
                                CredentialReference::Stored(stored_name)
                            } else {
                                CredentialReference::Environment(
                                    provider
                                        .environment_variable()
                                        .unwrap_or_default()
                                        .to_owned(),
                                )
                            }
                        }
                    };
                    (
                        ResolvedAuth::ApiKey(secret).into_http()?,
                        "api_key".to_owned(),
                        credential,
                    )
                }
                BuiltinAuth::OpenAiCodex => {
                    let profile = builtin_auth::profile_name(profile.as_deref());
                    (
                        HttpAuth::RequestTimeCodex(
                            self.inner.credentials.codex_request_credentials(profile),
                        ),
                        "codex".to_owned(),
                        CredentialReference::Profile(profile.to_owned()),
                    )
                }
                BuiltinAuth::XAi => {
                    let profile_name = builtin_auth::profile_name(profile.as_deref());
                    let credential = api_key.as_ref().map_or_else(
                        || CredentialReference::Profile(profile_name.to_owned()),
                        credential_reference,
                    );
                    (
                        HttpAuth::RequestTimeBearer(
                            self.inner
                                .credentials
                                .xai_request_credentials(profile_name, api_key.clone()),
                        ),
                        "bearer".to_owned(),
                        credential,
                    )
                }
            },
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
            HttpProviderRecipe::new(endpoint, protocol, auth)
                .with_headers(headers)
                .with_body_shape(responses_body_shape(access))
                .with_cache_routing(cache_routing(access, api)),
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

const LEGACY_DEFAULT_MAX_OUTPUT_TOKENS: [u32; 2] = [2_048, 4_096];

const CLAUDE_DEFAULT_REASONING_EFFORT: qq_provider::ReasoningEffort =
    qq_provider::ReasoningEffort::Medium;

fn with_claude_default_effort(
    snapshot: ConfigSnapshot,
    live_efforts: Option<&[qq_provider::ReasoningEffort]>,
) -> ConfigSnapshot {
    let Some(provider) = snapshot.providers().get(snapshot.model().provider()) else {
        return snapshot;
    };
    let Some(access) = provider.access() else {
        return snapshot;
    };
    let model = snapshot.model().model();
    let metadata = provider.models().get(model);
    let api = effective_provider_api(provider, model, access);
    let claude = anthropic_model_behind_a_gateway(
        model,
        metadata.and_then(qq_config::ModelMetadata::canonical_id),
    );
    let accepts_effort = matches!(access, ProviderAccess::Http(_))
        && matches!(
            api,
            ProviderApi::OpenAiChatCompletions | ProviderApi::AnthropicMessages
        )
        && metadata.is_none_or(qq_config::ModelMetadata::reasoning)
        && live_efforts.map_or_else(
            || {
                let configured = metadata
                    .map(qq_config::ModelMetadata::reasoning_efforts)
                    .filter(|levels| !levels.is_empty());
                configured
                    .or_else(|| {
                        qq_config::builtin_reasoning_efforts(
                            metadata
                                .and_then(qq_config::ModelMetadata::canonical_id)
                                .unwrap_or(model),
                        )
                    })
                    .is_none_or(|levels| levels.contains(&CLAUDE_DEFAULT_REASONING_EFFORT))
            },
            |levels| levels.contains(&CLAUDE_DEFAULT_REASONING_EFFORT),
        );
    if claude && accepts_effort {
        snapshot.with_default_reasoning_effort(CLAUDE_DEFAULT_REASONING_EFFORT)
    } else {
        snapshot
    }
}

fn anthropic_model_behind_a_gateway(wire_id: &str, canonical_id: Option<&str>) -> bool {
    match canonical_id {
        Some(canonical) => canonical.starts_with("anthropic/"),
        None => {
            let id = wire_id.to_ascii_lowercase();
            id.starts_with("claude-")
                || id.starts_with("anthropic/")
                || id.contains(".anthropic.")
                || id.starts_with("anthropic.")
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

fn model_in_catalog(
    provider: &ProviderConfig,
    model: &str,
    discovered: Option<&[DiscoveredModel]>,
) -> bool {
    provider.models().get(model).is_some_and(|metadata| {
        metadata.explicitly_configured()
            || !provider.kind().discovery().is_authoritative()
            || discovered.is_none()
    }) || discovered.is_some_and(|models| models.iter().any(|candidate| candidate.id == model))
}

#[derive(Clone, Copy)]
enum RouteAdmission<'a> {
    Authenticated,
    CredentialPresent(&'a BTreeSet<String>),
}

fn provider_credential_present(provider: &ProviderConfig, registered: &BTreeSet<String>) -> bool {
    let reference_present = |reference: &qq_config::SecretRef| match reference {
        qq_config::SecretRef::Env(variable) => environment_variable_set(variable),
        qq_config::SecretRef::Stored(name) => registered.contains(name.as_str()),
        qq_config::SecretRef::Value(_) => true,
    };
    match provider.access() {
        Some(ProviderAccess::Http(access)) => match access.auth() {
            HttpCredential::Configured(auth) => match auth {
                ProviderAuth::NoAuth => true,
                ProviderAuth::ApiKey(reference)
                | ProviderAuth::Bearer(reference)
                | ProviderAuth::Header(_, reference) => reference_present(reference),
            },
            HttpCredential::Builtin {
                provider,
                api_key,
                profile,
            } => match api_key {
                Some(reference) => reference_present(reference),
                None => {
                    registered.contains(&provider.stored_name(profile.as_deref()))
                        || provider
                            .variables
                            .iter()
                            .copied()
                            .any(environment_variable_set)
                }
            },
        },
        Some(
            ProviderAccess::AmazonBedrock { auth, .. }
            | ProviderAccess::AmazonBedrockMantle { auth, .. },
        ) => match auth {
            BedrockAuth::ApiKey(reference) => reference_present(reference),
            BedrockAuth::Aws(AwsAuth::Profile(profile)) => aws_profile_configured(profile),
            BedrockAuth::Aws(AwsAuth::DefaultChain) => aws_default_chain_source().is_some(),
        },
        None => false,
    }
}

fn environment_variable_set(variable: &str) -> bool {
    std::env::var_os(variable).is_some_and(|value| !value.is_empty())
}

pub fn aws_default_chain_source() -> Option<&'static str> {
    if std::env::var_os("AWS_ACCESS_KEY_ID").is_some()
        && std::env::var_os("AWS_SECRET_ACCESS_KEY").is_some()
    {
        return Some("AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY");
    }
    if std::env::var_os("AWS_PROFILE")
        .and_then(|profile| profile.into_string().ok())
        .is_some_and(|profile| aws_profile_configured(&profile))
    {
        return Some("AWS_PROFILE");
    }
    if std::env::var_os("AWS_WEB_IDENTITY_TOKEN_FILE").is_some()
        && std::env::var_os("AWS_ROLE_ARN").is_some()
    {
        return Some("web identity (AWS_WEB_IDENTITY_TOKEN_FILE and AWS_ROLE_ARN)");
    }
    if std::env::var_os("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI").is_some()
        || std::env::var_os("AWS_CONTAINER_CREDENTIALS_FULL_URI").is_some()
    {
        return Some("container credentials");
    }
    None
}

pub fn aws_profile_configured(profile: &str) -> bool {
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
                        model_is_fallback: false,
                        model: Some(worker.as_str().to_owned()),
                        max_output_tokens: chosen_max_output_tokens(
                            snapshot.max_output_tokens(),
                            snapshot.provenance(),
                        ),
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
        advertised: Arc<[String]>,
    ) -> SpawnModelValidationFuture {
        let factory = self.clone();
        Box::pin(async move {
            let build = tokio::task::spawn_blocking(move || {
                factory.validate_spawn_selection(&workspace, &selection, &advertised)
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
        self.load_with_progress(request, qq_core::RuntimeLoadProgress::default())
    }

    fn load_with_progress(
        &self,
        request: RuntimeLoadRequest,
        progress: qq_core::RuntimeLoadProgress,
    ) -> RuntimeLoadFuture {
        let execution_root = request.workspace.clone();
        self.load_in_execution_root(request, execution_root, progress)
    }

    fn load_in_execution_root(
        &self,
        request: RuntimeLoadRequest,
        execution_root: String,
        progress: qq_core::RuntimeLoadProgress,
    ) -> RuntimeLoadFuture {
        let execution_root = Some(PathBuf::from(execution_root));
        let factory = self.clone();
        Box::pin(async move {
            if progress.is_cancelled() {
                return Err(RuntimeLoadError {
                    kind: RunFailureKind::Server,
                    message: RuntimeBuildError::PreparationCancelled.to_string(),
                });
            }
            let completed = progress.clone();
            let build = tokio::task::spawn_blocking(move || {
                preparation_phase(
                    Some(&progress),
                    qq_core::RuntimeLoadStage::CanonicalizingWorkspace,
                )?;
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
                preparation_phase(
                    Some(&progress),
                    qq_core::RuntimeLoadStage::LoadingConfiguration,
                )?;
                let mut load =
                    factory.request_for_workspace(&workspace, request.model.max_output_tokens)?;
                let mut overrides = load.overrides().clone();
                if let Some(effort) = request.reasoning_effort {
                    overrides = overrides.with_reasoning_effort(effort);
                }
                if !request.model.model_is_fallback
                    && let Some(model) = request.model.model
                {
                    overrides = overrides.with_model(model);
                }
                if let Some(organization) = request.model.organization {
                    overrides = overrides.with_organization(organization);
                }
                load = load.with_overrides(overrides);
                let plan = factory
                    .plan_in_execution_root(
                        &load,
                        &request.profile,
                        Some(&progress),
                        execution_root.as_deref(),
                    )?
                    .0;
                Ok::<_, RuntimeBuildError>(LoadedRuntime::new(plan))
            })
            .await;
            completed.set(qq_core::RuntimeLoadStage::Complete);
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

enum ResolvedAuth {
    NoAuth,
    ApiKey(Secret),
    Bearer(Secret),
    Header(String, Secret),
}

fn credential_reference(reference: &SecretRef) -> CredentialReference {
    match reference {
        SecretRef::Env(name) => CredentialReference::Environment(name.clone()),
        SecretRef::Stored(name) => CredentialReference::Stored(name.clone()),
        SecretRef::Value(_) => CredentialReference::Inline,
    }
}

pub use crate::describe_endpoint;

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
    #[cfg(any(test, feature = "test-support"))]
    pub async fn open(factory: RuntimeFactory) -> Result<Self, RuntimeHandlerError> {
        Self::open_with(factory, None, None).await
    }

    pub async fn open_owned(
        factory: RuntimeFactory,
        approval_timeout: Option<std::time::Duration>,
        retention: Option<std::time::Duration>,
        max_active_runs: usize,
    ) -> Result<Self, RuntimeHandlerError> {
        factory.validate_isolated_tui_qa_state()?;
        let database_path = factory.inner.config.session_database_path()?;
        let options = SessionRuntimeOptions::new(database_path)
            .with_max_active_runs(max_active_runs)
            .with_approval_timeout(approval_timeout)
            .with_retention(retention);
        let durable = SessionRuntime::open(options, Arc::new(factory.clone())).await?;
        Ok(Self { durable, factory })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub async fn open_with(
        factory: RuntimeFactory,
        approval_timeout: Option<std::time::Duration>,
        retention: Option<std::time::Duration>,
    ) -> Result<Self, RuntimeHandlerError> {
        Self::open_owned(
            factory,
            approval_timeout,
            retention,
            qq_config::default_max_active_runs(),
        )
        .await
    }

    pub fn sessions(&self) -> &SessionRuntime {
        &self.durable
    }

    #[must_use]
    pub fn server_identity(&self, display_name: Option<&str>) -> ServerIdentity {
        ServerIdentity::new(self.durable.store_id(), display_name)
    }

    pub async fn shutdown(&self) -> Result<(), RuntimeHandlerError> {
        self.durable.shutdown().await?;
        self.factory.shutdown_plans();
        Ok(())
    }

    pub async fn close(&self) -> Result<(), RuntimeHandlerError> {
        self.durable.close().await?;
        Ok(())
    }
}

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

    fn workspace_catalog(
        &self,
        request: qq_protocol::WorkspaceCatalogRequest,
    ) -> qq_server::WorkspaceCatalogFuture {
        let runtime = self.durable.clone();
        Box::pin(async move {
            runtime
                .workspace_catalog(request)
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
                let load = factory.request_for_workspace(Path::new(&workspace), None)?;
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
        error @ (SessionRuntimeError::ToolProjectionUnavailable { .. }
        | SessionRuntimeError::EmptyWorkspace
        | SessionRuntimeError::InvalidWorkspace
        | SessionRuntimeError::EmptyPrompt
        | SessionRuntimeError::PromptTooLarge
        | SessionRuntimeError::InvalidSlashCommand(_)
        | SessionRuntimeError::InvalidSideQuestion
        | SessionRuntimeError::SideQuestionCancelled
        | SessionRuntimeError::SideQuestionTimedOut
        | SessionRuntimeError::InvalidRunLimits
        | SessionRuntimeError::InvalidOutputContract(_)
        | SessionRuntimeError::InvalidInput(_)
        | SessionRuntimeError::RunNotSteerable
        | SessionRuntimeError::UnknownProfile(_)
        | SessionRuntimeError::NoCompactionToRollBack
        | SessionRuntimeError::WorkspaceNotFound
        | SessionRuntimeError::SessionNotFound
        | SessionRuntimeError::SessionActive
        | SessionRuntimeError::WorkspaceBusy
        | SessionRuntimeError::UndoFailed(_)
        | SessionRuntimeError::ParentWorkspaceMismatch
        | SessionRuntimeError::RunNotFound
        | SessionRuntimeError::SideQuestionNotFound
        | SessionRuntimeError::ToolCallNotFound
        | SessionRuntimeError::ApprovalNotPending
        | SessionRuntimeError::ChildAuthorityEscalation
        | SessionRuntimeError::ChildDepthExceeded
        | SessionRuntimeError::DescendantLimitReached
        | SessionRuntimeError::ContextTooLarge
        | SessionRuntimeError::EventTooLarge
        | SessionRuntimeError::InvalidModelSelection
        | SessionRuntimeError::IdempotencyConflict
        | SessionRuntimeError::CursorStoreMismatch
        | SessionRuntimeError::CursorExpired
        | SessionRuntimeError::CursorWorkspaceMismatch
        | SessionRuntimeError::InvalidPageLimit) => {
            ServerHandlerError::InvalidRequest(error.to_string())
        }
        SessionRuntimeError::SideQuestionBusy
        | SessionRuntimeError::QueueFull
        | SessionRuntimeError::SteeringQueueFull
        | SessionRuntimeError::WorkspaceLimitReached
        | SessionRuntimeError::SessionLimitReached
        | SessionRuntimeError::CommandLimitReached
        | SessionRuntimeError::Overloaded => ServerHandlerError::Unavailable,
        SessionRuntimeError::InvalidRunLimit
        | SessionRuntimeError::SnapshotTooLarge
        | SessionRuntimeError::OutputTooLarge
        | SessionRuntimeError::AccountingUnavailable
        | SessionRuntimeError::ShutdownTimedOut
        | SessionRuntimeError::Unavailable
        | SessionRuntimeError::StoreBusy
        | SessionRuntimeError::UnsupportedStoreSchema { .. }
        | SessionRuntimeError::Persistence(_) => ServerHandlerError::Internal,
    }
}

#[derive(Debug, Error)]
pub enum RuntimeHandlerError {
    #[error(transparent)]
    Build(#[from] RuntimeBuildError),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Sessions(#[from] SessionRuntimeError),
}

fn delegation_roster(
    snapshot: &ConfigSnapshot,
    current: &qq_config::ModelRoute,
    discovered_efforts: impl Fn(&qq_config::ModelRoute) -> Option<Vec<qq_provider::ReasoningEffort>>,
) -> qq_protocol::DelegationRoster {
    let metadata = |route: &qq_config::ModelRoute| {
        snapshot
            .providers()
            .get(route.provider())
            .and_then(|provider| provider.models().get(route.model()))
    };
    let blended = |pricing: &qq_config::ModelPricing| -> u128 {
        u128::from(pricing.input_usd_nanos_per_token) * 3
            + u128::from(pricing.output_usd_nanos_per_token)
    };
    let current_price = metadata(current)
        .and_then(qq_config::ModelMetadata::pricing)
        .map(blended)
        .filter(|price| *price > 0);
    let config = snapshot.delegation();
    let parent_effort = snapshot.reasoning_effort();
    qq_protocol::DelegationRoster {
        roster: config
            .roster()
            .iter()
            .map(|entry| {
                let metadata = metadata(entry.route());
                let mut roster_entry = qq_protocol::DelegationRosterEntry {
                    route: entry.route().as_str().to_owned(),
                    role: delegation_role(entry.role()),
                    note: entry.note().map(str::to_owned),
                    effort: entry.effort(),
                    context_window: snapshot
                        .providers()
                        .get(entry.route().provider())
                        .and_then(|provider| provider.model_context_window(entry.route().model())),
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
                };
                if roster_entry.effort.is_none()
                    && snapshot
                        .providers()
                        .get(entry.route().provider())
                        .is_some_and(|provider| {
                            provider.access().is_some_and(|access| {
                                !matches!(access, ProviderAccess::Http(_))
                                    || !matches!(
                                        effective_provider_api(
                                            provider,
                                            entry.route().model(),
                                            access
                                        ),
                                        ProviderApi::OpenAiResponses
                                            | ProviderApi::OpenAiChatCompletions
                                            | ProviderApi::AnthropicMessages
                                    )
                            })
                        })
                {
                    roster_entry.effort = Some(qq_provider::ReasoningEffort::Default);
                }
                if roster_entry.effort.is_none()
                    && let Some(derived) =
                        qq_protocol::child_reasoning_effort(&roster_entry, parent_effort)
                {
                    use qq_provider::ReasoningEffort;
                    let live_efforts = discovered_efforts(entry.route());
                    let static_efforts = metadata
                        .map(qq_config::ModelMetadata::reasoning_efforts)
                        .filter(|levels| !levels.is_empty());
                    let known_efforts = static_efforts.or_else(|| {
                        qq_config::builtin_reasoning_efforts(
                            metadata
                                .and_then(qq_config::ModelMetadata::canonical_id)
                                .unwrap_or(entry.route().model()),
                        )
                    });
                    let effective_efforts = live_efforts.as_deref().or(known_efforts);
                    let ladder = effective_efforts.unwrap_or(&[]);
                    let rank = |effort: ReasoningEffort| {
                        ReasoningEffort::ALL
                            .iter()
                            .position(|level| *level == effort)
                    };
                    roster_entry.effort = if derived == ReasoningEffort::Default
                        || effective_efforts.is_some_and(<[_]>::is_empty)
                        || metadata.is_some_and(|metadata| !metadata.reasoning())
                    {
                        Some(ReasoningEffort::Default)
                    } else if live_efforts.is_none() && metadata.is_none() {
                        None
                    } else if ladder.is_empty() || ladder.contains(&derived) {
                        Some(derived)
                    } else {
                        let derived_rank = rank(derived);
                        ladder
                            .iter()
                            .copied()
                            .filter(|level| rank(*level) <= derived_rank)
                            .max_by_key(|level| rank(*level))
                            .or_else(|| ladder.iter().copied().min_by_key(|level| rank(*level)))
                    };
                }
                roster_entry
            })
            .collect(),
        default_role: delegation_role(config.default_role()),
        max_depth: config.max_depth(),
        write_children: config.write_children(),
    }
}

fn network_policy(policy: &qq_config::EffectivePolicy) -> qq_core::NetworkPolicy {
    qq_core::NetworkPolicy {
        deny_hosts: policy.deny_hosts().to_vec().into(),
        allow_private_for_tests: false,
    }
}

fn shell_policy(
    policy: &qq_config::EffectivePolicy,
    shell: &qq_config::ShellConfig,
) -> qq_core::ShellPolicy {
    qq_core::ShellPolicy {
        login_environment: shell.login_environment,
        deny_prefixes: policy.deny_shell_prefixes().to_vec().into(),
        builtin_preference: match policy.builtin_preference() {
            qq_config::BuiltinPreference::Off => qq_core::BuiltinPreference::Off,
            qq_config::BuiltinPreference::Hint => qq_core::BuiltinPreference::Hint,
            qq_config::BuiltinPreference::Strict => qq_core::BuiltinPreference::Strict,
        },
    }
}

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
                HttpCredential::Builtin { provider, .. } => {
                    let auth = BuiltinAuth::of(provider);
                    update_digest(&mut digest, auth.digest_marker());
                    if let BuiltinAuth::ApiKey { audience } = auth {
                        update_digest(&mut digest, audience.as_bytes());
                    }
                }
            }
            digest_body_shape(&mut digest, access.auth(), responses_body_shape(access));
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

fn cache_routing(access: &HttpAccess, api: ProviderApi) -> CacheRouting {
    let HttpCredential::Builtin { provider, .. } = access.auth() else {
        return CacheRouting::None;
    };
    match (provider.id, api) {
        ("openai" | "openai-codex", ProviderApi::OpenAiResponses) => CacheRouting::PromptCacheKey,
        ("xai", ProviderApi::OpenAiChatCompletions) => CacheRouting::GrokConversation,
        _ => CacheRouting::None,
    }
}

fn responses_body_shape(access: &HttpAccess) -> ResponsesBodyShape {
    implied_body_shape(access.auth())
}

fn implied_body_shape(auth: &HttpCredential) -> ResponsesBodyShape {
    match auth {
        HttpCredential::Builtin { provider, .. }
            if BuiltinAuth::of(provider) == BuiltinAuth::OpenAiCodex =>
        {
            ResponsesBodyShape::Codex
        }
        HttpCredential::Builtin { .. } | HttpCredential::Configured(_) => {
            ResponsesBodyShape::Standard
        }
    }
}

fn digest_body_shape(digest: &mut Sha256, auth: &HttpCredential, shape: ResponsesBodyShape) {
    let implied = implied_body_shape(auth);
    if shape != implied {
        update_digest(
            digest,
            match shape {
                ResponsesBodyShape::Standard => b"standard-body",
                ResponsesBodyShape::Codex => b"codex-body",
            },
        );
    }
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

fn preparation_phase(
    progress: Option<&qq_core::RuntimeLoadProgress>,
    stage: qq_core::RuntimeLoadStage,
) -> Result<(), RuntimeBuildError> {
    if let Some(progress) = progress {
        if progress.is_cancelled() {
            return Err(RuntimeBuildError::PreparationCancelled);
        }
        progress.set(stage);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum RuntimeBuildError {
    #[error("runtime preparation was cancelled")]
    PreparationCancelled,
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
    McpBridge(#[from] crate::McpBuildError),
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
    #[error(transparent)]
    Plan(#[from] PlanCompileError),
    #[error(transparent)]
    CatalogClientUnavailable(#[from] crate::catalog::ModelDiscoveryError),
    #[error(
        "model route {0:?} uses a provider API that does not support reasoning_effort; remove that setting or choose a supported route"
    )]
    UnsupportedReasoningEffort(String),
    #[error(
        "model route {model:?} does not advertise reasoning_effort {}; it accepts {}",
        effort.as_str(),
        advertised.iter().map(|effort| effort.as_str()).collect::<Vec<_>>().join(", ")
    )]
    ReasoningEffortNotAdvertised {
        model: String,
        effort: qq_provider::ReasoningEffort,
        advertised: Vec<qq_provider::ReasoningEffort>,
    },
    #[error("isolated TUI QA profile is invalid: {reason}")]
    InvalidTuiQaProfile { reason: String },
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
            | Self::McpBridge(crate::McpBuildError::Configuration(_))
            | Self::UnknownModel { .. }
            | Self::UnknownProfile(_)
            | Self::UnsupportedReasoningEffort(_)
            | Self::ReasoningEffortNotAdvertised { .. }
            | Self::InvalidTuiQaProfile { .. } => RunFailureKind::Configuration,
            Self::UnauthenticatedProvider(_) => RunFailureKind::Authentication,
            Self::Runtime(_)
            | Self::UnknownProvider(_)
            | Self::IncompleteProvider(_)
            | Self::UnsupportedApi { .. }
            | Self::UnrepresentableOutputLimit { .. } => RunFailureKind::ProviderConfiguration,
            Self::Plan(_) => RunFailureKind::Configuration,
            Self::PreparationCancelled
            | Self::McpBridge(crate::McpBuildError::CacheUnavailable)
            | Self::CacheUnavailable
            | Self::PlanCacheFull
            | Self::PlanCacheShutDown
            | Self::CatalogClientUnavailable(_) => RunFailureKind::Server,
        }
    }
}

#[cfg(test)]
mod tests;
