//! Layered application configuration loading and validation.

#![forbid(unsafe_code)]

use std::{
    collections::BTreeMap,
    env, fmt,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use thiserror::Error;

mod document;
mod loader;
mod managed;
mod models;
mod promote;
mod providers;
mod remote;
mod tui;

pub use qq_provider::{SecretLiteral, SecretRef, XAI_CREDENTIAL_ENDPOINT};
pub use tui::{
    TuiAction, TuiConfigDefaults, TuiConfigSettings, TuiConfigSnapshot, TuiLayout, TuiSourceReport,
};

pub const DEFAULT_MAX_OUTPUT_TOKENS: u32 = 4_096;
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MCP_CALL_TIMEOUT_SECONDS: u64 = 60;
pub const MAX_MCP_CALL_TIMEOUT_SECONDS: u64 = 600;
pub const DEFAULT_MCP_MAX_CONCURRENT_CALLS: u32 = 4;
pub const MAX_MCP_MAX_CONCURRENT_CALLS: u32 = 64;

/// All process-dependent inputs captured before a configuration load begins.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct LoadRequest {
    cwd: PathBuf,
    explicit_path: Option<PathBuf>,
    explicit_content: Option<String>,
    overrides: RuntimeOverrides,
}

impl LoadRequest {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            ..Self::default()
        }
    }

    /// Captures all supported environment variables without silently dropping
    /// non-Unicode values. Secret environment variables are intentionally not read.
    pub fn from_process_env(
        cwd: impl Into<PathBuf>,
        max_output_tokens: Option<u32>,
    ) -> Result<Self, ConfigError> {
        let mut request = Self::new(cwd);
        request.explicit_path = optional_environment("QQ_CONFIG")?.map(PathBuf::from);
        request.explicit_content = optional_environment("QQ_CONFIG_CONTENT")?;
        request.overrides.model = optional_environment("QQ_MODEL")?;
        request.overrides.organization = optional_environment("QQ_ORGANIZATION")?;
        request.overrides.max_output_tokens = max_output_tokens;
        Ok(request)
    }

    pub fn from_current_process(max_output_tokens: Option<u32>) -> Result<Self, ConfigError> {
        let cwd = env::current_dir().map_err(|error| ConfigError::CurrentDirectory { error })?;
        Self::from_process_env(cwd, max_output_tokens)
    }

    #[must_use]
    pub fn with_explicit_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.explicit_path = Some(path.into());
        self
    }

    #[must_use]
    pub fn with_explicit_content(mut self, content: impl Into<String>) -> Self {
        self.explicit_content = Some(content.into());
        self
    }

    #[must_use]
    pub fn with_overrides(mut self, overrides: RuntimeOverrides) -> Self {
        self.overrides = overrides;
        self
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[must_use]
    pub fn explicit_path(&self) -> Option<&Path> {
        self.explicit_path.as_deref()
    }

    #[must_use]
    pub fn has_explicit_content(&self) -> bool {
        self.explicit_content.is_some()
    }

    #[must_use]
    pub const fn overrides(&self) -> &RuntimeOverrides {
        &self.overrides
    }
}

impl fmt::Debug for LoadRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoadRequest")
            .field("cwd", &self.cwd)
            .field("explicit_path", &self.explicit_path)
            .field(
                "explicit_content",
                &self.explicit_content.as_ref().map(|_| "<redacted>"),
            )
            .field("overrides", &self.overrides)
            .finish()
    }
}

fn optional_environment(name: &'static str) -> Result<Option<String>, ConfigError> {
    match env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(env::VarError::NotPresent) => Ok(None),
        Err(env::VarError::NotUnicode(_)) => Err(ConfigError::NonUnicodeEnvironment(name)),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuntimeOverrides {
    organization: Option<String>,
    model: Option<String>,
    max_output_tokens: Option<u32>,
}

impl RuntimeOverrides {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_organization(mut self, organization: impl Into<String>) -> Self {
        self.organization = Some(organization.into());
        self
    }

    #[must_use]
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    #[must_use]
    pub const fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    #[must_use]
    pub fn organization(&self) -> Option<&str> {
        self.organization.as_deref()
    }

    #[must_use]
    pub fn model(&self) -> Option<&str> {
        self.model.as_deref()
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }

    fn is_empty(&self) -> bool {
        self.organization.is_none() && self.model.is_none() && self.max_output_tokens.is_none()
    }
}

/// Injectable roots for global configuration, trust data, and managed policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigPaths {
    global_dir: PathBuf,
    data_dir: PathBuf,
    managed_dir: PathBuf,
    enforce_managed_ownership: bool,
}

impl ConfigPaths {
    #[must_use]
    pub fn new(
        global_dir: impl Into<PathBuf>,
        data_dir: impl Into<PathBuf>,
        managed_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            global_dir: global_dir.into(),
            data_dir: data_dir.into(),
            managed_dir: managed_dir.into(),
            enforce_managed_ownership: false,
        }
    }

    #[must_use]
    fn with_managed_ownership_checks(mut self) -> Self {
        self.enforce_managed_ownership = true;
        self
    }

    pub fn system() -> Result<Self, ConfigError> {
        loader::system_paths()
    }

    #[must_use]
    pub fn global_dir(&self) -> &Path {
        &self.global_dir
    }

    #[must_use]
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    #[must_use]
    pub fn managed_dir(&self) -> &Path {
        &self.managed_dir
    }

    #[must_use]
    pub fn trust_file(&self) -> PathBuf {
        self.data_dir.join("trust.ron")
    }

    #[must_use]
    pub fn trust_lock_file(&self) -> PathBuf {
        self.data_dir.join("trust.lock")
    }

    #[must_use]
    pub fn organizations_file(&self) -> PathBuf {
        self.data_dir.join("organizations.ron")
    }

    #[must_use]
    pub fn organizations_lock_file(&self) -> PathBuf {
        self.data_dir.join("organizations.lock")
    }

    #[must_use]
    pub fn organizations_cache_dir(&self) -> PathBuf {
        self.data_dir.join("organizations")
    }
}

#[derive(Clone)]
pub struct ConfigLoader {
    paths: ConfigPaths,
    mdm_reader: Arc<dyn managed::MdmReader>,
}

impl fmt::Debug for ConfigLoader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ConfigLoader")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

impl ConfigLoader {
    #[must_use]
    pub fn new(paths: ConfigPaths) -> Self {
        Self {
            paths,
            mdm_reader: Arc::new(managed::SystemMdmReader),
        }
    }

    pub fn system() -> Result<Self, ConfigError> {
        Ok(Self::new(ConfigPaths::system()?))
    }

    #[must_use]
    pub const fn paths(&self) -> &ConfigPaths {
        &self.paths
    }

    pub fn load(&self, request: &LoadRequest) -> Result<ConfigSnapshot, ConfigError> {
        loader::load(self, request)
    }

    pub fn load_tui<Validate, ValidationError>(
        &self,
        cwd: &Path,
        defaults: &TuiConfigDefaults,
        validate_binding: Validate,
    ) -> Result<TuiConfigSnapshot, ConfigError>
    where
        Validate: Fn(&str) -> Result<(), ValidationError>,
        ValidationError: fmt::Display,
    {
        tui::load(self, cwd, defaults, &validate_binding)
    }

    /// Resolves the durable session database owned by the configured user data directory.
    pub fn session_database_path(&self) -> Result<PathBuf, ConfigError> {
        loader::ensure_data_directory(self.paths.data_dir())?;
        Ok(self.paths.data_dir().join("sessions.sqlite3"))
    }

    /// Grants every currently pending project source digest in one atomic state update.
    pub fn grant_pending_trust(
        &self,
        request: &LoadRequest,
    ) -> Result<Vec<PendingTrust>, ConfigError> {
        loader::grant_pending_trust(self, request)
    }

    pub fn enroll_organization(
        &self,
        name: &str,
        manifest_url: &str,
    ) -> Result<OrganizationEnrollment, ConfigError> {
        remote::enroll(&self.paths, name, manifest_url)
    }

    pub fn refresh_organization(&self, name: &str) -> Result<OrganizationEnrollment, ConfigError> {
        remote::refresh(&self.paths, name)
    }

    pub fn select_organization(&self, name: &str) -> Result<(), ConfigError> {
        remote::select(&self.paths, name)
    }

    pub fn remove_organization(&self, name: &str) -> Result<bool, ConfigError> {
        remote::remove(&self.paths, name)
    }

    pub fn organizations(&self) -> Result<Vec<OrganizationEnrollment>, ConfigError> {
        remote::list(&self.paths)
    }

    /// Durably records an approval grant in the workspace's
    /// `.qq/config.ron` policy section, creating the file or section when
    /// absent and preserving unrelated user content otherwise. The write is
    /// atomic (temp file + rename), idempotent for a grant the document
    /// already declares, and refused when the managed layer denies the grant.
    pub fn promote_workspace_grant(
        &self,
        workspace_dir: &Path,
        grant: &WorkspaceGrant,
    ) -> Result<GrantPromotion, ConfigError> {
        promote::promote_workspace_grant(self, workspace_dir, grant)
    }

    #[cfg(test)]
    fn with_mdm_reader(mut self, reader: Arc<dyn managed::MdmReader>) -> Self {
        self.mdm_reader = reader;
        self
    }
}

pub fn load(request: &LoadRequest) -> Result<ConfigSnapshot, ConfigError> {
    ConfigLoader::system()?.load(request)
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StaticHeaderValue(String);

impl StaticHeaderValue {
    #[must_use]
    pub fn expose_value(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for StaticHeaderValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderApi {
    OpenAiResponses,
    OpenAiChatCompletions,
    AnthropicMessages,
    GoogleGenerateContent,
    BedrockConverse,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndpointMode {
    Base,
    Exact,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderAuth {
    NoAuth,
    ApiKey(SecretRef),
    Bearer(SecretRef),
    Header(String, SecretRef),
}

impl ProviderAuth {
    fn contains_literal_secret(&self) -> bool {
        match self {
            Self::NoAuth => false,
            Self::ApiKey(secret) | Self::Bearer(secret) | Self::Header(_, secret) => {
                secret.is_literal()
            }
        }
    }

    const fn references_local_credential(&self) -> bool {
        !matches!(self, Self::NoAuth)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AwsAuth {
    DefaultChain,
    Profile(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BedrockAuth {
    Aws(AwsAuth),
    ApiKey(SecretRef),
}

impl BedrockAuth {
    fn contains_literal_secret(&self) -> bool {
        matches!(self, Self::ApiKey(secret) if secret.is_literal())
    }

    const fn references_local_credential(&self) -> bool {
        matches!(self, Self::Aws(AwsAuth::Profile(_)) | Self::ApiKey(_))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    base_url: String,
    api: ProviderApi,
    auth: ProviderAuth,
    #[serde(default, deserialize_with = "document::deserialize_unique_btree_map")]
    headers: BTreeMap<String, StaticHeaderValue>,
}

impl Connection {
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    #[must_use]
    pub const fn api(&self) -> ProviderApi {
        self.api
    }

    #[must_use]
    pub const fn auth(&self) -> &ProviderAuth {
        &self.auth
    }

    #[must_use]
    pub const fn headers(&self) -> &BTreeMap<String, StaticHeaderValue> {
        &self.headers
    }

    fn contains_literal_secret(&self) -> bool {
        self.auth.contains_literal_secret() || !self.headers.is_empty()
    }

    const fn references_local_credential(&self) -> bool {
        self.auth.references_local_credential()
    }
}

/// How a configured MCP server is reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpTransport {
    /// Spawn `command args...` and speak MCP over its stdio. `env` lists
    /// environment variables passed through from the server process; the
    /// child otherwise starts from a cleared environment (plus `PATH` and
    /// `HOME`).
    Stdio {
        command: String,
        args: Vec<String>,
        env: Vec<String>,
    },
    /// Streamable-HTTP endpoint; the bearer token is sourced like every
    /// other secret in the configuration system.
    Http {
        url: String,
        bearer: Option<SecretRef>,
    },
}

/// One configuration-declared MCP server, merged across layers by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerConfig {
    transport: McpTransport,
    eager: bool,
    allow: Vec<String>,
    call_timeout_seconds: u64,
    max_concurrent_calls: u32,
}

impl McpServerConfig {
    pub(crate) const fn new(
        transport: McpTransport,
        eager: bool,
        allow: Vec<String>,
        call_timeout_seconds: u64,
        max_concurrent_calls: u32,
    ) -> Self {
        Self {
            transport,
            eager,
            allow,
            call_timeout_seconds,
            max_concurrent_calls,
        }
    }

    #[must_use]
    pub const fn transport(&self) -> &McpTransport {
        &self.transport
    }

    /// Connect at server startup instead of on first use.
    #[must_use]
    pub const fn eager(&self) -> bool {
        self.eager
    }

    /// Bare tool names allowlisted by configuration; they become
    /// `mcp__<server>__<tool>` grants in the approval policy.
    #[must_use]
    pub fn allow(&self) -> &[String] {
        &self.allow
    }

    #[must_use]
    pub const fn call_timeout_seconds(&self) -> u64 {
        self.call_timeout_seconds
    }

    #[must_use]
    pub const fn max_concurrent_calls(&self) -> u32 {
        self.max_concurrent_calls
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrganizationEnrollment {
    name: String,
    manifest_url: String,
    selected: bool,
}

impl OrganizationEnrollment {
    fn new(name: String, manifest_url: String, selected: bool) -> Self {
        Self {
            name,
            manifest_url,
            selected,
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn manifest_url(&self) -> &str {
        &self.manifest_url
    }

    #[must_use]
    pub const fn selected(&self) -> bool {
        self.selected
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputModality {
    Text,
    Image,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModelMetadata {
    canonical_id: Option<String>,
    api: Option<ProviderApi>,
    name: Option<String>,
    reasoning: bool,
    input: Vec<InputModality>,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
    pricing: Option<ModelPricing>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

impl ModelMetadata {
    #[must_use]
    pub fn canonical_id(&self) -> Option<&str> {
        self.canonical_id.as_deref()
    }

    #[must_use]
    pub const fn api(&self) -> Option<ProviderApi> {
        self.api
    }

    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    #[must_use]
    pub const fn reasoning(&self) -> bool {
        self.reasoning
    }

    #[must_use]
    pub fn input(&self) -> &[InputModality] {
        &self.input
    }

    #[must_use]
    pub const fn context_window(&self) -> Option<u32> {
        self.context_window
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }

    #[must_use]
    pub const fn pricing(&self) -> Option<&ModelPricing> {
        self.pricing.as_ref()
    }

    pub(crate) fn builtin(
        canonical_id: &str,
        api: Option<ProviderApi>,
        name: &str,
        reasoning: bool,
        context_window: u32,
        max_output_tokens: u32,
        pricing: Option<ModelPricing>,
    ) -> Self {
        Self {
            canonical_id: Some(canonical_id.to_owned()),
            api,
            name: Some(name.to_owned()),
            reasoning,
            input: vec![InputModality::Text],
            context_window: Some(context_window),
            max_output_tokens: Some(max_output_tokens),
            pricing,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAi,
    OpenAiCodex,
    Anthropic,
    Google,
    XAi,
    LiteLlm,
    AmazonBedrock,
    AmazonBedrockMantle,
    Custom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageType {
    Metered,
    Subscription,
    Unknown,
    CredentialDependent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpCredential {
    Configured(ProviderAuth),
    ApiKey {
        explicit: Option<SecretRef>,
        stored_name: &'static str,
        environment_variable: &'static str,
        audience: &'static str,
    },
    OpenAiCodex {
        profile: Option<String>,
    },
    XAi {
        api_key: Option<SecretRef>,
        profile: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpAccess {
    endpoint: String,
    endpoint_mode: EndpointMode,
    api: ProviderApi,
    auth: HttpCredential,
    headers: BTreeMap<String, StaticHeaderValue>,
}

impl HttpAccess {
    #[doc(hidden)]
    pub fn new(
        endpoint: impl Into<String>,
        endpoint_mode: EndpointMode,
        api: ProviderApi,
        auth: HttpCredential,
        headers: BTreeMap<String, StaticHeaderValue>,
    ) -> Self {
        Self {
            endpoint: endpoint.into(),
            endpoint_mode,
            api,
            auth,
            headers,
        }
    }

    pub(crate) fn configured(connection: &Connection) -> Self {
        Self::new(
            connection.base_url.clone(),
            EndpointMode::Base,
            connection.api,
            HttpCredential::Configured(connection.auth.clone()),
            connection.headers.clone(),
        )
    }

    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[must_use]
    pub const fn endpoint_mode(&self) -> EndpointMode {
        self.endpoint_mode
    }

    #[must_use]
    pub const fn api(&self) -> ProviderApi {
        self.api
    }

    #[must_use]
    pub const fn auth(&self) -> &HttpCredential {
        &self.auth
    }

    #[must_use]
    pub const fn headers(&self) -> &BTreeMap<String, StaticHeaderValue> {
        &self.headers
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProviderAccess {
    Http(HttpAccess),
    AmazonBedrock {
        region: Option<String>,
        auth: BedrockAuth,
    },
    AmazonBedrockMantle {
        region: Option<String>,
        api: ProviderApi,
        auth: BedrockAuth,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderConfig {
    kind: ProviderKind,
    access: Option<ProviderAccess>,
    usage: UsageType,
    models: BTreeMap<String, ModelMetadata>,
}

impl ProviderConfig {
    #[doc(hidden)]
    pub fn new(
        kind: ProviderKind,
        access: Option<ProviderAccess>,
        usage: UsageType,
        models: BTreeMap<String, ModelMetadata>,
    ) -> Self {
        Self {
            kind,
            access,
            usage,
            models,
        }
    }

    pub(crate) fn models_mut(&mut self) -> &mut BTreeMap<String, ModelMetadata> {
        &mut self.models
    }

    pub(crate) fn access_mut(&mut self) -> &mut Option<ProviderAccess> {
        &mut self.access
    }

    #[must_use]
    pub fn models(&self) -> &BTreeMap<String, ModelMetadata> {
        &self.models
    }

    #[must_use]
    pub const fn kind(&self) -> ProviderKind {
        self.kind
    }

    #[must_use]
    pub const fn access(&self) -> Option<&ProviderAccess> {
        self.access.as_ref()
    }

    #[must_use]
    pub const fn usage(&self) -> UsageType {
        self.usage
    }

    #[must_use]
    pub const fn connection(&self) -> Option<&Connection> {
        None
    }

    #[must_use]
    pub const fn uses_custom_endpoint(&self) -> bool {
        matches!(self.kind, ProviderKind::LiteLlm | ProviderKind::Custom)
    }

    fn contains_literal_secret(&self) -> bool {
        match self.access.as_ref() {
            Some(ProviderAccess::Http(access)) => {
                (match &access.auth {
                    HttpCredential::Configured(auth) => auth.contains_literal_secret(),
                    HttpCredential::ApiKey { explicit, .. }
                    | HttpCredential::XAi {
                        api_key: explicit, ..
                    } => explicit.as_ref().is_some_and(SecretRef::is_literal),
                    HttpCredential::OpenAiCodex { .. } => false,
                }) || !access.headers.is_empty()
            }
            Some(
                ProviderAccess::AmazonBedrock { auth, .. }
                | ProviderAccess::AmazonBedrockMantle { auth, .. },
            ) => auth.contains_literal_secret(),
            None => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelRoute {
    full: String,
    provider: String,
    model: String,
}

impl ModelRoute {
    fn parse(value: String) -> Result<Self, ConfigError> {
        let Some((provider, model)) = value.split_once('/') else {
            return Err(ConfigError::InvalidModelRoute(value));
        };
        if provider.is_empty() || model.is_empty() {
            return Err(ConfigError::InvalidModelRoute(value));
        }
        Ok(Self {
            full: value.clone(),
            provider: provider.to_owned(),
            model: model.to_owned(),
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.full
    }

    #[must_use]
    pub fn provider(&self) -> &str {
        &self.provider
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectivePolicy {
    allowed_providers: Option<Vec<String>>,
    denied_providers: Vec<String>,
    max_output_tokens: Option<u32>,
    require_https: bool,
    allow_custom_providers: bool,
    allow_literal_secrets: bool,
    allow_tools: Vec<String>,
    allow_shell_prefixes: Vec<String>,
    deny_tools: Vec<String>,
    deny_shell_prefixes: Vec<String>,
}

impl Default for EffectivePolicy {
    fn default() -> Self {
        Self {
            allowed_providers: None,
            denied_providers: Vec::new(),
            max_output_tokens: None,
            require_https: false,
            allow_custom_providers: true,
            allow_literal_secrets: true,
            allow_tools: Vec::new(),
            allow_shell_prefixes: Vec::new(),
            deny_tools: Vec::new(),
            deny_shell_prefixes: Vec::new(),
        }
    }
}

impl EffectivePolicy {
    #[must_use]
    pub fn allowed_providers(&self) -> Option<&[String]> {
        self.allowed_providers.as_deref()
    }

    #[must_use]
    pub fn denied_providers(&self) -> &[String] {
        &self.denied_providers
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<u32> {
        self.max_output_tokens
    }

    #[must_use]
    pub const fn require_https(&self) -> bool {
        self.require_https
    }

    #[must_use]
    pub const fn allow_custom_providers(&self) -> bool {
        self.allow_custom_providers
    }

    #[must_use]
    pub const fn allow_literal_secrets(&self) -> bool {
        self.allow_literal_secrets
    }

    /// Exact tool names granted across layers, before deny filtering and
    /// before per-MCP-server allowlists are folded in. Prefer
    /// [`ConfigSnapshot::grants`] for the resolved set.
    #[must_use]
    pub fn allow_tools(&self) -> &[String] {
        &self.allow_tools
    }

    /// Shell command prefixes granted across layers, before deny filtering.
    /// Prefer [`ConfigSnapshot::grants`] for the resolved set.
    #[must_use]
    pub fn allow_shell_prefixes(&self) -> &[String] {
        &self.allow_shell_prefixes
    }

    /// Managed-only: exact tool names filtered out of the effective grants.
    #[must_use]
    pub fn deny_tools(&self) -> &[String] {
        &self.deny_tools
    }

    /// Managed-only: shell prefixes whose word-granularity overlap filters
    /// lower-layer shell grants.
    #[must_use]
    pub fn deny_shell_prefixes(&self) -> &[String] {
        &self.deny_shell_prefixes
    }
}

/// The resolved workspace grant set: exact tool names (with per-MCP-server
/// allowlists folded in as `mcp__<server>__<tool>`) and shell command
/// prefixes, after managed deny filtering. Grants are not secrets; the values
/// render unredacted. Session creation seeds its grant set from this.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyGrants {
    tools: Vec<String>,
    shell_prefixes: Vec<String>,
}

impl PolicyGrants {
    pub(crate) const fn new(tools: Vec<String>, shell_prefixes: Vec<String>) -> Self {
        Self {
            tools,
            shell_prefixes,
        }
    }

    /// Exact tool names allowed to run without prompting, sorted and deduped.
    #[must_use]
    pub fn tools(&self) -> &[String] {
        &self.tools
    }

    /// Shell command prefixes matched at word granularity, sorted and deduped.
    #[must_use]
    pub fn shell_prefixes(&self) -> &[String] {
        &self.shell_prefixes
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty() && self.shell_prefixes.is_empty()
    }
}

/// One approval grant with workspace lifetime: an exact tool name (built-in
/// or `mcp__<server>__<tool>`) or a shell command prefix matched at word
/// granularity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceGrant {
    Tool(String),
    ShellPrefix(String),
}

impl WorkspaceGrant {
    #[must_use]
    pub fn value(&self) -> &str {
        match self {
            Self::Tool(value) | Self::ShellPrefix(value) => value,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromotionOutcome {
    /// The grant was appended and durably written.
    Added,
    /// The document already declared the grant; nothing was written.
    AlreadyPresent,
}

/// Result of promoting a grant into workspace configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantPromotion {
    path: PathBuf,
    outcome: PromotionOutcome,
}

impl GrantPromotion {
    pub(crate) const fn new(path: PathBuf, outcome: PromotionOutcome) -> Self {
        Self { path, outcome }
    }

    /// The workspace configuration file that carries the grant.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    #[must_use]
    pub const fn outcome(&self) -> PromotionOutcome {
        self.outcome
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SourceKind {
    Compiled,
    Remote,
    Global,
    Project,
    Explicit,
    Inline,
    Runtime,
    Managed,
    Mdm,
    TrustState,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceIdentity {
    kind: SourceKind,
    path: Option<PathBuf>,
    label: String,
}

impl SourceIdentity {
    fn virtual_source(kind: SourceKind, label: impl Into<String>) -> Self {
        Self {
            kind,
            path: None,
            label: label.into(),
        }
    }

    fn file(kind: SourceKind, path: PathBuf) -> Self {
        let label = path.display().to_string();
        Self {
            kind,
            path: Some(path),
            label,
        }
    }

    #[must_use]
    pub const fn kind(&self) -> SourceKind {
        self.kind
    }

    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    #[must_use]
    pub fn label(&self) -> &str {
        &self.label
    }
}

impl fmt::Display for SourceIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.label)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceStatus {
    Applied,
    PartiallyAppliedPendingTrust,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfigKey {
    Organization,
    Model,
    WorkerModel,
    ReviewerModel,
    MaxOutputTokens,
    Providers,
    Provider(String),
    Policy,
    Mcp,
    McpServer(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceReport {
    source: SourceIdentity,
    status: SourceStatus,
    touched: Vec<ConfigKey>,
}

impl SourceReport {
    fn new(source: SourceIdentity, status: SourceStatus, touched: Vec<ConfigKey>) -> Self {
        Self {
            source,
            status,
            touched,
        }
    }

    #[must_use]
    pub const fn source(&self) -> &SourceIdentity {
        &self.source
    }

    #[must_use]
    pub const fn status(&self) -> SourceStatus {
        self.status
    }

    #[must_use]
    pub fn touched(&self) -> &[ConfigKey] {
        &self.touched
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigProvenance {
    organization: Option<SourceIdentity>,
    model: Option<SourceIdentity>,
    worker_model: Option<SourceIdentity>,
    reviewer_model: Option<SourceIdentity>,
    max_output_tokens: Option<SourceIdentity>,
    providers: BTreeMap<String, SourceIdentity>,
    grant_tools: BTreeMap<String, SourceIdentity>,
    grant_shell_prefixes: BTreeMap<String, SourceIdentity>,
}

impl ConfigProvenance {
    #[must_use]
    pub const fn organization(&self) -> Option<&SourceIdentity> {
        self.organization.as_ref()
    }

    #[must_use]
    pub const fn model(&self) -> Option<&SourceIdentity> {
        self.model.as_ref()
    }

    #[must_use]
    pub const fn worker_model(&self) -> Option<&SourceIdentity> {
        self.worker_model.as_ref()
    }

    #[must_use]
    pub const fn reviewer_model(&self) -> Option<&SourceIdentity> {
        self.reviewer_model.as_ref()
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> Option<&SourceIdentity> {
        self.max_output_tokens.as_ref()
    }

    #[must_use]
    pub fn provider(&self, name: &str) -> Option<&SourceIdentity> {
        self.providers.get(name)
    }

    #[must_use]
    pub const fn providers(&self) -> &BTreeMap<String, SourceIdentity> {
        &self.providers
    }

    /// The layer that last declared the tool grant still in effect.
    #[must_use]
    pub fn grant_tool(&self, name: &str) -> Option<&SourceIdentity> {
        self.grant_tools.get(name)
    }

    /// The layer that last declared the shell-prefix grant still in effect.
    #[must_use]
    pub fn grant_shell_prefix(&self, prefix: &str) -> Option<&SourceIdentity> {
        self.grant_shell_prefixes.get(prefix)
    }

    #[must_use]
    pub const fn grant_tools(&self) -> &BTreeMap<String, SourceIdentity> {
        &self.grant_tools
    }

    #[must_use]
    pub const fn grant_shell_prefixes(&self) -> &BTreeMap<String, SourceIdentity> {
        &self.grant_shell_prefixes
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingTrust {
    source: SourceIdentity,
    digest: String,
}

impl PendingTrust {
    fn new(source: SourceIdentity, digest: String) -> Self {
        Self { source, digest }
    }

    #[must_use]
    pub const fn source(&self) -> &SourceIdentity {
        &self.source
    }

    #[must_use]
    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// A fully merged, validated configuration. All fields are read-only to callers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigSnapshot {
    organization: Option<String>,
    model: ModelRoute,
    worker_model: Option<ModelRoute>,
    reviewer_model: Option<ModelRoute>,
    max_output_tokens: u32,
    providers: BTreeMap<String, ProviderConfig>,
    mcp: BTreeMap<String, McpServerConfig>,
    policy: EffectivePolicy,
    grants: PolicyGrants,
    reports: Vec<SourceReport>,
    provenance: ConfigProvenance,
}

impl ConfigSnapshot {
    #[must_use]
    pub fn organization(&self) -> Option<&str> {
        self.organization.as_deref()
    }

    #[must_use]
    pub const fn model(&self) -> &ModelRoute {
        &self.model
    }

    #[must_use]
    pub const fn worker_model(&self) -> Option<&ModelRoute> {
        self.worker_model.as_ref()
    }

    /// The model route that adjudicates held tool approvals, when configured.
    #[must_use]
    pub const fn reviewer_model(&self) -> Option<&ModelRoute> {
        self.reviewer_model.as_ref()
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }

    #[must_use]
    pub const fn providers(&self) -> &BTreeMap<String, ProviderConfig> {
        &self.providers
    }

    /// Configuration-declared MCP servers by name.
    #[must_use]
    pub const fn mcp_servers(&self) -> &BTreeMap<String, McpServerConfig> {
        &self.mcp
    }

    #[must_use]
    pub const fn policy(&self) -> &EffectivePolicy {
        &self.policy
    }

    /// The resolved workspace grant set consulted at session creation:
    /// declared tool and shell-prefix grants plus folded per-MCP-server
    /// allowlists, after managed deny filtering.
    #[must_use]
    pub const fn grants(&self) -> &PolicyGrants {
        &self.grants
    }

    #[must_use]
    pub fn source_reports(&self) -> &[SourceReport] {
        &self.reports
    }

    #[must_use]
    pub const fn provenance(&self) -> &ConfigProvenance {
        &self.provenance
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("the platform configuration directories are unavailable")]
    SystemDirectoriesUnavailable,
    #[error("failed to determine the current directory: {error}")]
    CurrentDirectory {
        #[source]
        error: std::io::Error,
    },
    #[error("environment variable {0} is not valid Unicode")]
    NonUnicodeEnvironment(&'static str),
    #[error("configuration working directory is invalid: {path}")]
    InvalidWorkingDirectory { path: PathBuf },
    #[error("explicit configuration file does not exist: {path}")]
    ExplicitConfigMissing { path: PathBuf },
    #[error("symbolic links are not accepted as configuration sources: {path}")]
    SymlinkSource { path: PathBuf },
    #[error("configuration source is not a regular file: {path}")]
    NotRegularFile { path: PathBuf },
    #[error("configuration source is not a directory: {path}")]
    NotDirectory { path: PathBuf },
    #[error("configuration fragment name is invalid: {path}")]
    InvalidFragmentName { path: PathBuf },
    #[error("configuration file was discovered more than once: {path}")]
    DuplicateSource { path: PathBuf },
    #[error("configuration containing literal secrets is not private: {path}")]
    InsecureSecretFile { path: PathBuf },
    #[error("managed configuration is not administrator-owned and protected: {path}")]
    InsecureManagedSource { path: PathBuf },
    #[error("failed to read MDM configuration from {origin}: {message}")]
    MdmRead { origin: String, message: String },
    #[error("MDM configuration at {origin} must be a string")]
    InvalidMdmValue { origin: String },
    #[error("configuration state is not private: {path}")]
    InsecureStatePermissions { path: PathBuf },
    #[error("organization name is invalid; use 1-64 lowercase letters, digits, dots, or hyphens")]
    InvalidOrganizationName,
    #[error(
        "organization manifest URL must be an HTTPS URL without credentials, query, or fragment"
    )]
    InvalidOrganizationManifestUrl,
    #[error("organization {0:?} is not enrolled")]
    OrganizationNotEnrolled(String),
    #[error("organization {name:?} enrollment changed while its manifest was refreshing")]
    OrganizationEnrollmentChanged { name: String },
    #[error("organization {name:?} has no cached manifest; run `qq org refresh {name}`")]
    OrganizationManifestMissing { name: String },
    #[error("organization manifest for {name:?} must set `organization` to exactly that name")]
    OrganizationManifestMismatch { name: String },
    #[error("organization state uses unsupported version {version}; expected 1")]
    UnsupportedOrganizationStateVersion { version: u32 },
    #[error("organization state contains duplicate enrollment {name:?}")]
    DuplicateOrganizationEnrollment { name: String },
    #[error("failed to fetch manifest for organization {name:?}: {message}")]
    OrganizationFetch { name: String, message: String },
    #[error("organization {name:?} manifest server returned HTTP {status}")]
    OrganizationHttpStatus { name: String, status: u16 },
    #[error("configuration source exceeds the {limit}-byte limit: {origin}")]
    SourceTooLarge {
        origin: SourceIdentity,
        limit: usize,
    },
    #[error("inline configuration exceeds the {limit}-byte limit")]
    InlineSourceTooLarge { limit: usize },
    #[error("failed to access {path}: {error}")]
    Io {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
    #[error("configuration source is not valid UTF-8: {origin}")]
    InvalidUtf8 { origin: SourceIdentity },
    #[error("failed to parse configuration source {origin}: {message}")]
    Parse {
        origin: SourceIdentity,
        message: String,
    },
    #[error("configuration source {origin} has unsupported version {version}; expected 1")]
    UnsupportedVersion {
        origin: SourceIdentity,
        version: u32,
    },
    #[error("managed-only policy settings are only allowed in managed configuration: {origin}")]
    PolicyOutsideManaged { origin: SourceIdentity },
    #[error("remote configuration cannot declare approval grants: {origin}")]
    RemotePolicyGrantsForbidden { origin: SourceIdentity },
    #[error("workspace grant is invalid: {message}")]
    InvalidGrant { message: String },
    #[error("workspace grant {grant:?} is denied by managed policy {rule}")]
    GrantDeniedByManaged { grant: String, rule: &'static str },
    #[error("literal secret values are forbidden in {origin}")]
    LiteralSecretForbidden { origin: SourceIdentity },
    #[error("remote configuration cannot select local credential references: {origin}")]
    RemoteCredentialReferenceForbidden { origin: SourceIdentity },
    #[error("remote configuration cannot declare MCP servers: {origin}")]
    RemoteMcpForbidden { origin: SourceIdentity },
    #[error("project configuration trust is required")]
    TrustRequired {
        pending: Vec<PendingTrust>,
        reports: Vec<SourceReport>,
    },
    #[error("failed to serialize configuration state: {message}")]
    StateSerialization { message: String },
    #[error("trust state has unsupported version {version}; expected 1")]
    UnsupportedTrustVersion { version: u32 },
    #[error("trust state contains a duplicate record for {path} and {digest}")]
    DuplicateTrustRecord { path: PathBuf, digest: String },
    #[error("trust state contains an invalid SHA-256 digest: {digest}")]
    InvalidTrustDigest { digest: String },
    #[error("model must be configured")]
    ModelRequired,
    #[error("model route must use provider/model syntax: {0:?}")]
    InvalidModelRoute(String),
    #[error("model route selects an unknown or disabled provider: {0}")]
    UnknownProvider(String),
    #[error("managed policy {rule} was violated: {message}")]
    PolicyViolation { rule: &'static str, message: String },
    #[error("TUI settings are invalid: {message}")]
    InvalidTuiSettings { message: String },
    #[error(
        "the current binary must integrate ConfigSnapshot and resolve its SecretRef externally"
    )]
    LegacyIntegrationRequired,
}

/// Compatibility for the current binary until provider construction consumes
/// `ConfigSnapshot` and resolves `SecretRef` outside this module.
pub struct AppConfig {
    pub openai_api_key: String,
    pub model: String,
    pub max_output_tokens: u32,
}

impl AppConfig {
    pub fn from_env() -> Result<Self, ConfigError> {
        Err(ConfigError::LegacyIntegrationRequired)
    }
}

#[cfg(test)]
mod tests;
