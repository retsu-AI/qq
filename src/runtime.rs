//! Application configuration to model-runtime composition.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use qq_auth::{AuthError, CredentialStore, Secret, resolve_provider_credential};
use qq_config::{
    AwsAuth, BedrockAuth, ConfigError, ConfigLoader, ConfigSnapshot, EndpointMode, HttpAccess,
    HttpCredential, LoadRequest, PromotionOutcome, ProviderAccess, ProviderApi, ProviderAuth,
    ProviderConfig, WorkspaceGrant,
};
use qq_core::{
    ApprovalReviewer, GrantPromotionFuture, GrantSeedFuture, LoadedRuntime, ReviewFuture,
    ReviewRequest, ReviewVerdict, Runtime, RuntimeConfigError, RuntimeLoadError, RuntimeLoadFuture,
    RuntimeLoadRequest, RuntimeLoader, SessionEventStream, SessionRuntime, SessionRuntimeError,
    SessionRuntimeOptions, SpawnModelValidationFuture, WorkerRuntimeLoadFuture,
    WorkspaceGrantAuthority, WorkspaceGrantSeed,
};
use qq_protocol::{
    ApprovalGrant, CommandRequest, ModelCatalogRequest, ModelDescriptor, RunFailureKind,
    SnapshotRequest, SubscribeRequest, WorkspaceGrantOutcome,
};
use qq_provider::{
    BedrockAuth as ProviderBedrockAuth, EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe,
    ProviderCompiler, ProviderError, ProviderRecipe,
};
use qq_server::{CommandFuture, ModelsFuture, ServerHandler, ServerHandlerError, SnapshotFuture};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::catalog::{DiscoveredModel, ModelDiscovery};

const MAX_CACHED_RUNTIMES: usize = 16;
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
    cache: Mutex<VecDeque<(RuntimeKey, Arc<Runtime>)>>,
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
                cache: Mutex::new(VecDeque::new()),
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

    pub fn runtime_for(&self, request: &LoadRequest) -> Result<Arc<Runtime>, RuntimeBuildError> {
        let snapshot = self.load(request)?;
        self.runtime_for_snapshot(&snapshot)
    }

    pub fn runtime_for_snapshot(
        &self,
        snapshot: &ConfigSnapshot,
    ) -> Result<Arc<Runtime>, RuntimeBuildError> {
        self.runtime_with_key_for_snapshot(snapshot)
            .map(|(runtime, _)| runtime)
    }

    fn runtime_with_key_for_snapshot(
        &self,
        snapshot: &ConfigSnapshot,
    ) -> Result<(Arc<Runtime>, RuntimeKey), RuntimeBuildError> {
        let provider_id = snapshot.model().provider();
        let provider_config = snapshot
            .providers()
            .get(provider_id)
            .ok_or_else(|| RuntimeBuildError::UnknownProvider(provider_id.to_owned()))?;
        let (recipe, provider_key) =
            self.prepare_provider(provider_id, snapshot.model().model(), provider_config)?;
        // Configured MCP servers ride the runtime: the shared registry (one
        // client per server, cached by declaration digest) attaches here, and
        // its digest joins the cache key so declaration changes rebuild.
        let mcp = self
            .inner
            .mcp
            .registry_for_snapshot(&self.inner.credentials, snapshot)?;
        let (mcp_registry, mcp_key) = match mcp {
            Some((registry, key)) => (Some(registry), key),
            None => (None, Vec::new()),
        };
        let key = RuntimeKey::new(
            provider_id,
            snapshot.model().model(),
            snapshot.max_output_tokens(),
            &provider_key,
            &mcp_key,
        );

        {
            let mut cache = self
                .inner
                .cache
                .lock()
                .map_err(|_| RuntimeBuildError::CacheUnavailable)?;
            if let Some(runtime) = promote_cached_runtime(&mut cache, &key) {
                return Ok((runtime, key));
            }
        }

        let mut runtime = Runtime::with_provider(
            self.inner.providers.compile(recipe)?,
            snapshot.model().model(),
            snapshot.max_output_tokens(),
        )?;
        if let Some(registry) = mcp_registry {
            runtime = runtime.with_mcp_registry(registry);
        }
        let runtime = Arc::new(runtime);

        let mut cache = self
            .inner
            .cache
            .lock()
            .map_err(|_| RuntimeBuildError::CacheUnavailable)?;
        if let Some(existing) = promote_cached_runtime(&mut cache, &key) {
            return Ok((existing, key));
        }
        cache.push_back((key.clone(), Arc::clone(&runtime)));
        while cache.len() > MAX_CACHED_RUNTIMES {
            cache.pop_front();
        }
        Ok((runtime, key))
    }

    fn prepare_provider(
        &self,
        provider_id: &str,
        model_id: &str,
        config: &ProviderConfig,
    ) -> Result<(ProviderRecipe, Vec<u8>), RuntimeBuildError> {
        let access = config
            .access()
            .ok_or_else(|| RuntimeBuildError::IncompleteProvider(provider_id.to_owned()))?;
        match access {
            ProviderAccess::Http(access) => {
                let api = config
                    .models()
                    .get(model_id)
                    .and_then(|metadata| metadata.api())
                    .unwrap_or(access.api());
                self.prepare_http_provider(provider_id, access, api)
            }
            ProviderAccess::AmazonBedrock { region, auth } => {
                self.prepare_bedrock_provider(provider_id, region.as_deref(), auth)
            }
            ProviderAccess::AmazonBedrockMantle { region, api, auth } => {
                let api = config
                    .models()
                    .get(model_id)
                    .and_then(|metadata| metadata.api())
                    .unwrap_or(*api);
                self.prepare_bedrock_mantle_provider(provider_id, region.as_deref(), api, auth)
            }
        }
    }

    fn prepare_http_provider(
        &self,
        provider_id: &str,
        access: &HttpAccess,
        api: ProviderApi,
    ) -> Result<(ProviderRecipe, Vec<u8>), RuntimeBuildError> {
        let prepared_auth = match access.auth() {
            HttpCredential::Configured(auth) => {
                PreparedHttpAuth::Static(self.resolve_http_auth(auth, access.endpoint())?)
            }
            HttpCredential::ApiKey {
                explicit,
                stored_name,
                environment_variable,
                audience,
            } => PreparedHttpAuth::Static(ResolvedAuth::ApiKey(resolve_provider_credential(
                &self.inner.credentials,
                explicit.as_ref(),
                stored_name,
                environment_variable,
                Some(audience),
            )?)),
            HttpCredential::OpenAiCodex { profile } => {
                let profile = profile.as_deref().unwrap_or("default");
                PreparedHttpAuth::RequestTime {
                    auth: HttpAuth::RequestTimeCodex(
                        self.inner.credentials.codex_request_credentials(profile),
                    ),
                    key_auth: ResolvedAuth::NoAuth,
                    identity: vec![("credential-profile".to_owned(), profile.to_owned())],
                }
            }
            HttpCredential::XAi { api_key, profile } => {
                let profile = profile.as_deref().unwrap_or("default");
                let key_auth = match api_key.as_ref() {
                    Some(reference) => {
                        ResolvedAuth::Bearer(self.inner.credentials.resolve_with_endpoint(
                            reference,
                            Some(qq_config::XAI_CREDENTIAL_ENDPOINT),
                        )?)
                    }
                    None => ResolvedAuth::NoAuth,
                };
                PreparedHttpAuth::RequestTime {
                    auth: HttpAuth::RequestTimeBearer(
                        self.inner
                            .credentials
                            .xai_request_credentials(profile, api_key.clone()),
                    ),
                    key_auth,
                    identity: vec![("credential-profile".to_owned(), profile.to_owned())],
                }
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
        let mut key_headers = headers.clone();
        key_headers.extend(prepared_auth.identity().iter().cloned());
        let key = provider_key(
            provider_id,
            access.endpoint(),
            endpoint_mode,
            provider_api_name(api),
            prepared_auth.key_auth(),
            key_headers
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str())),
        );
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
            HttpProviderRecipe::new(endpoint, protocol, prepared_auth.into_http()?)
                .with_headers(headers),
        );
        Ok((recipe, key))
    }

    fn prepare_bedrock_provider(
        &self,
        provider_id: &str,
        region: Option<&str>,
        auth: &BedrockAuth,
    ) -> Result<(ProviderRecipe, Vec<u8>), RuntimeBuildError> {
        let credential_endpoint =
            region.map(|region| format!("https://bedrock-runtime.{region}.amazonaws.com"));
        let key_endpoint = credential_endpoint
            .as_deref()
            .unwrap_or("aws-region-provider-chain");

        let (auth, key) = match auth {
            BedrockAuth::Aws(AwsAuth::DefaultChain) => {
                let key = provider_key(
                    provider_id,
                    key_endpoint,
                    "aws",
                    "bedrock_converse",
                    &ResolvedAuth::NoAuth,
                    [("aws-auth", "default-chain")],
                );
                (ProviderBedrockAuth::DefaultChain, key)
            }
            BedrockAuth::Aws(AwsAuth::Profile(profile)) => {
                let key = provider_key(
                    provider_id,
                    key_endpoint,
                    "aws",
                    "bedrock_converse",
                    &ResolvedAuth::NoAuth,
                    [("aws-auth", "profile"), ("aws-profile", profile)],
                );
                (ProviderBedrockAuth::Profile(profile.clone()), key)
            }
            BedrockAuth::ApiKey(reference) => {
                let secret = self
                    .inner
                    .credentials
                    .resolve_with_endpoint(reference, credential_endpoint.as_deref())?;
                let api_key = secret.expose_secret_str()?.to_owned();
                let key_auth = ResolvedAuth::ApiKey(secret);
                let key = provider_key(
                    provider_id,
                    key_endpoint,
                    "aws",
                    "bedrock_converse",
                    &key_auth,
                    std::iter::empty::<(&str, &str)>(),
                );
                (ProviderBedrockAuth::ApiKey(api_key.into()), key)
            }
        };

        Ok((
            ProviderRecipe::amazon_bedrock(region.map(str::to_owned), auth),
            key,
        ))
    }

    fn prepare_bedrock_mantle_provider(
        &self,
        provider_id: &str,
        region: Option<&str>,
        api: ProviderApi,
        auth: &BedrockAuth,
    ) -> Result<(ProviderRecipe, Vec<u8>), RuntimeBuildError> {
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
        let key_endpoint = credential_endpoint
            .as_deref()
            .unwrap_or("aws-region-provider-chain");
        let api_name = provider_api_name(api);

        let (auth, key) = match auth {
            BedrockAuth::Aws(AwsAuth::DefaultChain) => {
                let key = provider_key(
                    provider_id,
                    key_endpoint,
                    "aws",
                    api_name,
                    &ResolvedAuth::NoAuth,
                    [("aws-auth", "default-chain")],
                );
                (ProviderBedrockAuth::DefaultChain, key)
            }
            BedrockAuth::Aws(AwsAuth::Profile(profile)) => {
                let key = provider_key(
                    provider_id,
                    key_endpoint,
                    "aws",
                    api_name,
                    &ResolvedAuth::NoAuth,
                    [("aws-auth", "profile"), ("aws-profile", profile)],
                );
                (ProviderBedrockAuth::Profile(profile.clone()), key)
            }
            BedrockAuth::ApiKey(reference) => {
                let secret = self
                    .inner
                    .credentials
                    .resolve_with_endpoint(reference, credential_endpoint.as_deref())?;
                let api_key = secret.expose_secret_str()?.to_owned();
                let key_auth = ResolvedAuth::ApiKey(secret);
                let key = provider_key(
                    provider_id,
                    key_endpoint,
                    "aws",
                    api_name,
                    &key_auth,
                    std::iter::empty::<(&str, &str)>(),
                );
                (ProviderBedrockAuth::ApiKey(api_key.into()), key)
            }
        };

        Ok((
            ProviderRecipe::amazon_bedrock_mantle(region.map(str::to_owned), protocol, auth),
            key,
        ))
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
                let snapshot = factory.load(&load)?;
                let pricing = snapshot
                    .providers()
                    .get(snapshot.model().provider())
                    .and_then(|provider| provider.models().get(snapshot.model().model()))
                    .and_then(|metadata| metadata.pricing())
                    .cloned()
                    .map(protocol_model_pricing);
                let spawn_model_routes = factory
                    .configured_model_options(&snapshot)
                    .into_iter()
                    .filter_map(|model| model.selection.model)
                    .collect();
                let runtime = Arc::new(
                    factory
                        .runtime_for_snapshot(&snapshot)?
                        .as_ref()
                        .clone()
                        .with_spawn_model_routes(spawn_model_routes),
                );
                Ok::<_, RuntimeBuildError>(LoadedRuntime { runtime, pricing })
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
            let written = tokio::task::spawn_blocking(move || {
                factory
                    .inner
                    .config
                    .promote_workspace_grant(&workspace, &grant)
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

/// One-shot reviewer verdict budget: a bounded, non-streaming-style read of
/// a small model's single JSON line.
const REVIEWER_MAX_OUTPUT_TOKENS: u32 = 512;
const REVIEWER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Adjudicates held tool approvals with the workspace-configured
/// `reviewer_model`, through the same provider compilation path as ordinary
/// runs. Every failure — no reviewer configured, config or provider errors,
/// timeout, unparseable verdict — resolves as `Escalate`, leaving the human
/// approval path untouched.
pub struct ModelApprovalReviewer {
    factory: RuntimeFactory,
}

impl ModelApprovalReviewer {
    pub fn new(factory: RuntimeFactory) -> Self {
        Self { factory }
    }

    fn escalate(reason: &str) -> ReviewVerdict {
        ReviewVerdict::Escalate {
            reason: reason.to_owned(),
        }
    }
}

impl ApprovalReviewer for ModelApprovalReviewer {
    fn review(&self, request: ReviewRequest) -> ReviewFuture {
        let factory = self.factory.clone();
        Box::pin(async move {
            let workspace = PathBuf::from(&request.workspace);
            let prepared = tokio::task::spawn_blocking(move || {
                let load = LoadRequest::from_process_env(&workspace, None)
                    .map_err(|error| error.to_string())?;
                let snapshot = factory.load(&load).map_err(|error| error.to_string())?;
                let Some(route) = snapshot.reviewer_model() else {
                    return Err("no reviewer model is configured".to_owned());
                };
                let provider_config = snapshot
                    .providers()
                    .get(route.provider())
                    .ok_or_else(|| format!("unknown reviewer provider {}", route.provider()))?;
                let (recipe, _) = factory
                    .prepare_provider(route.provider(), route.model(), provider_config)
                    .map_err(|error| error.to_string())?;
                let provider = factory
                    .inner
                    .providers
                    .compile(recipe)
                    .map_err(|error| error.to_string())?;
                Ok::<_, String>((provider, route.model().to_owned()))
            })
            .await;
            let (provider, model) = match prepared {
                Ok(Ok(prepared)) => prepared,
                Ok(Err(_)) | Err(_) => return Self::escalate("reviewer unavailable"),
            };
            let verdict = tokio::time::timeout(
                REVIEWER_TIMEOUT,
                collect_reviewer_verdict(provider, &model, &request),
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
const REVIEWER_SYSTEM_PROMPT: &str = "You are a tool-call safety reviewer for a coding agent. \
The agent proposed one action that static policy flagged as potentially dangerous. \
Judge only the action shown; you have no transcript and must not assume intent. \
Approve only actions whose worst plausible outcome is recoverable within the workspace \
(for example: deleting build artifacts, killing a process the workspace owns, \
reinstalling dependencies). Escalate anything externally visible, credential-touching, \
system-level, or ambiguous (for example: force-pushing shared branches, sudo, piped \
installers, writes outside the workspace). \
Reply with exactly one JSON object on one line and nothing else: \
{\"verdict\":\"approve\"} or {\"verdict\":\"escalate\",\"reason\":\"...\"} \
or {\"verdict\":\"deny\",\"reason\":\"...\"}.";

async fn collect_reviewer_verdict(
    provider: Arc<dyn qq_provider::Provider>,
    model: &str,
    request: &ReviewRequest,
) -> ReviewVerdict {
    use futures_util::StreamExt as _;
    use qq_provider::{ContentBlock, Message, ModelRequest, ProviderEvent, Role};

    let mut description = format!(
        "Tool: {}\nWorkspace: {}\n",
        request.tool_name, request.workspace
    );
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
        description.push_str(&edit.diff);
        description.push('\n');
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
    while let Some(event) = stream.next().await {
        match event {
            Ok(ProviderEvent::OutputTextDelta { text: delta }) => text.push_str(&delta),
            Ok(ProviderEvent::Completed { .. }) => break,
            Ok(_) => {}
            Err(_) => {
                return ReviewVerdict::Escalate {
                    reason: "reviewer request failed".to_owned(),
                };
            }
        }
    }
    parse_reviewer_verdict(&text)
}

/// Parses the reviewer's reply. The verdict must be the only JSON object in
/// the reply and `approve` carries no qualifier; everything else escalates.
fn parse_reviewer_verdict(text: &str) -> ReviewVerdict {
    #[derive(serde::Deserialize)]
    struct Reply {
        verdict: String,
        #[serde(default)]
        reason: Option<String>,
    }
    let trimmed = text.trim();
    let Ok(reply) = serde_json::from_str::<Reply>(trimmed) else {
        return ReviewVerdict::Escalate {
            reason: "reviewer reply was not a valid verdict".to_owned(),
        };
    };
    let reason = |reply: Reply| {
        reply
            .reason
            .unwrap_or_else(|| "reviewer verdict".to_owned())
    };
    match reply.verdict.as_str() {
        "approve" => ReviewVerdict::Approve,
        "deny" => ReviewVerdict::Deny {
            reason: reason(reply),
        },
        _ => ReviewVerdict::Escalate {
            reason: reason(reply),
        },
    }
}

fn promote_cached_runtime(
    cache: &mut VecDeque<(RuntimeKey, Arc<Runtime>)>,
    key: &RuntimeKey,
) -> Option<Arc<Runtime>> {
    let index = cache.iter().position(|(candidate, _)| candidate == key)?;
    let (cached_key, runtime) = cache
        .remove(index)
        .expect("a located runtime cache entry must exist");
    cache.push_back((cached_key, Arc::clone(&runtime)));
    Some(runtime)
}

enum ResolvedAuth {
    NoAuth,
    ApiKey(Secret),
    Bearer(Secret),
    Header(String, Secret),
}

enum PreparedHttpAuth {
    Static(ResolvedAuth),
    RequestTime {
        auth: HttpAuth,
        key_auth: ResolvedAuth,
        identity: Vec<(String, String)>,
    },
}

impl PreparedHttpAuth {
    fn key_auth(&self) -> &ResolvedAuth {
        match self {
            Self::Static(auth) => auth,
            Self::RequestTime { key_auth, .. } => key_auth,
        }
    }

    fn identity(&self) -> &[(String, String)] {
        match self {
            Self::Static(_) => &[],
            Self::RequestTime { identity, .. } => identity,
        }
    }

    fn into_http(self) -> Result<HttpAuth, AuthError> {
        match self {
            Self::Static(auth) => auth.into_http(),
            Self::RequestTime { auth, .. } => Ok(auth),
        }
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

    fn update_digest(&self, digest: &mut Sha256) {
        match self {
            Self::NoAuth => update_digest(digest, b"no_auth"),
            Self::ApiKey(secret) => {
                update_digest(digest, b"api_key");
                update_digest(digest, secret.expose_secret_bytes());
            }
            Self::Bearer(secret) => {
                update_digest(digest, b"bearer");
                update_digest(digest, secret.expose_secret_bytes());
            }
            Self::Header(name, secret) => {
                update_digest(digest, b"header");
                update_digest(digest, name.as_bytes());
                update_digest(digest, secret.expose_secret_bytes());
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
        self.durable.shutdown().await?;
        Ok(())
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

    fn subscribe(
        &self,
        request: SubscribeRequest,
    ) -> Result<SessionEventStream, ServerHandlerError> {
        self.durable
            .subscribe(request)
            .map_err(map_session_runtime_error)
    }
}

fn map_session_runtime_error(error: SessionRuntimeError) -> ServerHandlerError {
    match error {
        error @ (SessionRuntimeError::EmptyWorkspace
        | SessionRuntimeError::InvalidWorkspace
        | SessionRuntimeError::EmptyPrompt
        | SessionRuntimeError::PromptTooLarge
        | SessionRuntimeError::WorkspaceNotFound
        | SessionRuntimeError::SessionNotFound
        | SessionRuntimeError::SessionActive
        | SessionRuntimeError::ParentWorkspaceMismatch
        | SessionRuntimeError::RunNotFound
        | SessionRuntimeError::ToolCallNotFound
        | SessionRuntimeError::ApprovalNotPending
        | SessionRuntimeError::InvalidApprovalGrant
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

fn provider_key<'a>(
    provider: &str,
    endpoint: &str,
    endpoint_mode: &str,
    api: &str,
    auth: &ResolvedAuth,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<u8> {
    let mut digest = Sha256::new();
    update_digest(&mut digest, provider.as_bytes());
    update_digest(&mut digest, endpoint.as_bytes());
    update_digest(&mut digest, endpoint_mode.as_bytes());
    update_digest(&mut digest, api.as_bytes());
    auth.update_digest(&mut digest);
    for (name, value) in headers {
        update_digest(&mut digest, name.as_bytes());
        update_digest(&mut digest, value.as_bytes());
    }
    digest.finalize().to_vec()
}

fn update_digest(digest: &mut Sha256, value: &[u8]) {
    digest.update(value.len().to_le_bytes());
    digest.update(value);
}

#[derive(Clone, PartialEq, Eq)]
struct RuntimeKey([u8; 32]);

impl RuntimeKey {
    fn new(
        provider: &str,
        model: &str,
        max_output_tokens: u32,
        provider_key: &[u8],
        mcp_key: &[u8],
    ) -> Self {
        let mut digest = Sha256::new();
        update_digest(&mut digest, provider.as_bytes());
        update_digest(&mut digest, model.as_bytes());
        digest.update(max_output_tokens.to_le_bytes());
        update_digest(&mut digest, provider_key);
        update_digest(&mut digest, mcp_key);
        Self(digest.finalize().into())
    }
}

impl std::fmt::Debug for RuntimeKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RuntimeKey([REDACTED])")
    }
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
    #[error(transparent)]
    Mcp(#[from] qq_mcp::McpConfigError),
    #[error("runtime cache is unavailable")]
    CacheUnavailable,
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
            Self::Mcp(_) | Self::UnknownModel { .. } => RunFailureKind::Configuration,
            Self::UnauthenticatedProvider(_) => RunFailureKind::Authentication,
            Self::Runtime(_)
            | Self::UnknownProvider(_)
            | Self::IncompleteProvider(_)
            | Self::UnsupportedApi { .. } => RunFailureKind::ProviderConfiguration,
            Self::CacheUnavailable | Self::CatalogClientUnavailable(_) => RunFailureKind::Server,
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
    use qq_config::{ConfigPaths, RuntimeOverrides};
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
        fn load(&self, _request: RuntimeLoadRequest) -> RuntimeLoadFuture {
            let runtime = Arc::clone(&self.runtime);
            Box::pin(async move {
                Ok(LoadedRuntime {
                    runtime,
                    pricing: None,
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
                    prompt: "/review focus on cancellation".to_owned(),
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
                prompt: "/review focus on cancellation".to_owned(),
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
                .runtime_for(&request)
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
        factory.runtime_for(&anthropic).unwrap();

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
        factory.runtime_for(&google).unwrap();
    }

    #[test]
    fn constructs_xai_runtimes_for_model_selected_responses_and_chat_protocols() {
        let fixture = RuntimeFixture::new();
        let factory = fixture.factory();

        for model in ["grok-4.5", "grok-4.3"] {
            let request = fixture.request(format!(
                r#"(
                    version: 1,
                    model: "xai/{model}",
                    providers: {{
                        "xai": XAi(api_key: Value("xai-test-secret")),
                    }},
                )"#
            ));
            factory.runtime_for(&request).unwrap();
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

        fixture.factory().runtime_for(&request).unwrap();
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

        let first = factory.runtime_for(&request).unwrap();
        let second = factory.runtime_for(&request).unwrap();

        assert!(Arc::ptr_eq(&first, &second));
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
                .runtime_for(&request)
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

                factory.runtime_for(&request).unwrap_or_else(|error| {
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
                .runtime_for(&request)
                .err()
                .expect("unsupported Mantle API must fail");
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
        let first = factory.runtime_for(&base).unwrap();
        let reused = factory.runtime_for(&base).unwrap();
        let different_region = factory
            .runtime_for(&document(
                "us-west-2",
                "OpenAiResponses",
                "Aws(DefaultChain)",
            ))
            .unwrap();
        let different_api = factory
            .runtime_for(&document(
                "us-east-1",
                "AnthropicMessages",
                "Aws(DefaultChain)",
            ))
            .unwrap();
        let different_profile = factory
            .runtime_for(&document(
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
            .runtime_for(&document("openai-model"))
            .expect("per-model OpenAiResponses override must construct");
        factory
            .runtime_for(&document("anthropic-model"))
            .expect("provider default AnthropicMessages must construct");

        // The per-model API must reach Mantle preparation: an unsupported
        // override fails even though the provider default is supported.
        let error = factory
            .runtime_for(&document("rejected-model"))
            .err()
            .expect("unsupported per-model API must fail");
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
        let first = factory.runtime_for(&api_key).unwrap();
        let reused = factory.runtime_for(&api_key).unwrap();
        let different_auth = factory.runtime_for(&bearer).unwrap();

        assert!(Arc::ptr_eq(&first, &reused));
        assert!(!Arc::ptr_eq(&first, &different_auth));
    }

    #[test]
    fn cache_key_includes_custom_auth_header_name() {
        let secret = || Secret::from_secret_bytes(b"same-test-secret".to_vec());
        let first = provider_key(
            "custom",
            "https://example.test/v1/responses",
            "exact",
            "openai_responses",
            &ResolvedAuth::Header("x-first".to_owned(), secret()),
            [],
        );
        let second = provider_key(
            "custom",
            "https://example.test/v1/responses",
            "exact",
            "openai_responses",
            &ResolvedAuth::Header("x-second".to_owned(), secret()),
            [],
        );
        let different_endpoint_mode = provider_key(
            "custom",
            "https://example.test/v1/responses",
            "base",
            "openai_responses",
            &ResolvedAuth::Header("x-first".to_owned(), secret()),
            [],
        );

        assert_ne!(first, second);
        assert_ne!(first, different_endpoint_mode);
    }

    #[test]
    fn reviewer_verdict_parses_strictly_and_escalates_everything_else() {
        assert!(matches!(
            parse_reviewer_verdict(r#"{"verdict":"approve"}"#),
            ReviewVerdict::Approve
        ));
        assert!(matches!(
            parse_reviewer_verdict("  {\"verdict\":\"approve\"}\n"),
            ReviewVerdict::Approve
        ));
        assert!(matches!(
            parse_reviewer_verdict(r#"{"verdict":"deny","reason":"wipes home"}"#),
            ReviewVerdict::Deny { reason } if reason == "wipes home"
        ));
        assert!(matches!(
            parse_reviewer_verdict(r#"{"verdict":"escalate","reason":"unsure"}"#),
            ReviewVerdict::Escalate { reason } if reason == "unsure"
        ));
        // Unknown verdicts, prose-wrapped JSON, and non-JSON all escalate:
        // the reviewer can only ever expedite, never widen, an approval.
        assert!(matches!(
            parse_reviewer_verdict(r#"{"verdict":"allow"}"#),
            ReviewVerdict::Escalate { .. }
        ));
        assert!(matches!(
            parse_reviewer_verdict(r#"Sure! {"verdict":"approve"}"#),
            ReviewVerdict::Escalate { .. }
        ));
        assert!(matches!(
            parse_reviewer_verdict("approve"),
            ReviewVerdict::Escalate { .. }
        ));
        assert!(matches!(
            parse_reviewer_verdict(""),
            ReviewVerdict::Escalate { .. }
        ));
    }
}
