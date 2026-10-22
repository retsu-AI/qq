//! Built-in provider access routes.

use std::{collections::BTreeMap, sync::LazyLock};

use crate::{
    AwsAuth, BedrockAuth, EndpointMode, HttpAccess, HttpCredential, ProviderAccess, ProviderApi,
    ProviderConfig, ProviderKind, UsageType,
    models::{BuiltinCatalog, builtin_models},
};

pub(crate) const OPENAI_ENDPOINT: &str = "https://api.openai.com/v1/responses";
pub(crate) const OPENAI_CREDENTIAL_ENDPOINT: &str = "https://api.openai.com";
pub(crate) const ANTHROPIC_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
pub(crate) const ANTHROPIC_CREDENTIAL_ENDPOINT: &str = "https://api.anthropic.com";
pub(crate) const GOOGLE_ENDPOINT: &str = "https://generativelanguage.googleapis.com/v1beta";
pub(crate) const GOOGLE_CREDENTIAL_ENDPOINT: &str = "https://generativelanguage.googleapis.com";
pub(crate) const XAI_ENDPOINT: &str = "https://api.x.ai/v1";
const CODEX_RESPONSES_ENDPOINT: &str = "https://chatgpt.com/backend-api/codex/responses";

/// One built-in HTTP provider: everything that differs between them, as
/// data. Adding a preset adds a row here (ADR-0064); `ProviderKind`, the
/// configuration patch syntax, `builtin()`, the credential-variable list and
/// patch application read the table.
pub(crate) struct BuiltinHttpProvider {
    /// The provider id configuration documents use (`providers: {"<id>": ..}`).
    pub(crate) id: &'static str,
    /// The configuration patch variant (`OpenAi(api_key: ..)`) and the name
    /// `qq config` prints for the kind.
    config_name: &'static str,
    /// The patch fields this provider accepts, in serialisation order. Any
    /// other field is a parse error (ADR-0064 §2).
    pub(crate) patch_fields: &'static [&'static str],
    endpoint: &'static str,
    endpoint_mode: EndpointMode,
    api: ProviderApi,
    usage: UsageType,
    catalog: BuiltinCatalog,
    discovery: ModelDiscoveryShape,
    /// Who defines the credential's audience, and so which resolver reads it.
    audience: BuiltinAudience,
    /// The variables read when nothing is stored, in precedence order.
    variables: &'static [&'static str],
    /// How `qq auth login <id>` may obtain this provider's credential, in
    /// the order the CLI documents them; the first is the default.
    login: &'static [LoginMethod],
    /// The model `qq init` proposes for this provider.
    init_model: &'static str,
    /// Position in `qq auth login`, its error message and `qq init`:
    /// API-key providers first, the subscription last.
    display_order: u8,
}

/// One way `qq auth login` obtains a built-in provider's credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginMethod {
    /// Prompt for an API key and store it bound to the provider's audience.
    ApiKey,
    /// The OpenAI Codex loopback browser sign-in.
    OpenAiCodexBrowser,
    /// The OpenAI Codex device-code sign-in (`--device-auth`).
    OpenAiCodexDevice,
    /// The xAI device-code OAuth sign-in (`--oauth`).
    XAiOAuth,
}

/// How model discovery treats a built-in provider's listing endpoint, beyond
/// what its protocol already implies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelDiscoveryShape {
    /// The listing supplements the bundled catalog.
    Supplementary,
    /// The listing is authoritative: bundled models it omits are hidden
    /// unless configured explicitly, and a malformed listing is rejected.
    Authoritative,
    /// The OpenAI Codex listing: authoritative, under `models`, with a
    /// client version and per-model visibility.
    Codex,
}

impl ModelDiscoveryShape {
    /// Whether the discovered list replaces the bundled one.
    #[must_use]
    pub const fn is_authoritative(self) -> bool {
        matches!(self, Self::Authoritative | Self::Codex)
    }
}

/// The public view of one built-in HTTP provider's row: what the CLI, `qq
/// init`, the doctor and credential resolution need, with no
/// configuration types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BuiltinProvider {
    pub id: &'static str,
    /// The configuration patch variant name, e.g. `OpenAi`.
    pub config_name: &'static str,
    /// Where this provider's credential audience comes from.
    pub audience: BuiltinAudience,
    /// The variables read when nothing is stored, in precedence order: the
    /// canonical name first, then any aliases the vendor's SDKs also read.
    pub variables: &'static [&'static str],
    pub login: &'static [LoginMethod],
    pub init_model: &'static str,
    pub discovery: ModelDiscoveryShape,
    /// The endpoint requests go to, and whether it is a base or exact URL.
    pub endpoint: &'static str,
    pub endpoint_mode: EndpointMode,
}

impl BuiltinProvider {
    /// The canonical variable read when nothing is stored, if any.
    #[must_use]
    pub fn environment_variable(&self) -> Option<&'static str> {
        self.variables.first().copied()
    }

    /// The stored credential name for `profile`: `<id>/<profile>`, with
    /// `default` when none is configured. `qq auth login <id>` writes it.
    #[must_use]
    pub fn stored_name(&self, profile: Option<&str>) -> String {
        format!("{}/{}", self.id, profile.unwrap_or("default"))
    }
}

/// Where a built-in provider's credential audience is defined. API-key
/// presets carry theirs in the provider table. Providers whose credential
/// `qq-auth` owns (an OAuth flow, even one that also accepts an API key)
/// name that owner, and the composition root maps it to the `qq-auth`
/// constant (ADR-0064 §1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BuiltinAudience {
    Table(&'static str),
    OpenAiCodex,
    XAi,
}

/// Every built-in HTTP provider, in the order the CLI lists them (each
/// row's `display_order`). Every table row appears: there is no separate
/// registry to keep in step with the table.
pub fn builtin_providers() -> impl Iterator<Item = BuiltinProvider> {
    let mut rows: Vec<&'static BuiltinHttpProvider> = BUILTIN_HTTP_PROVIDERS.iter().collect();
    rows.sort_by_key(|row| row.display_order);
    rows.into_iter().map(BuiltinHttpProvider::public)
}

/// The built-in HTTP provider `id` names, if any.
#[must_use]
pub fn builtin_provider(id: &str) -> Option<BuiltinProvider> {
    BUILTIN_HTTP_PROVIDERS
        .iter()
        .find(|row| row.id == id)
        .map(BuiltinHttpProvider::public)
}

impl BuiltinHttpProvider {
    pub(crate) const fn public(&self) -> BuiltinProvider {
        BuiltinProvider {
            id: self.id,
            config_name: self.config_name,
            audience: self.audience,
            variables: self.variables,
            login: self.login,
            init_model: self.init_model,
            discovery: self.discovery,
            endpoint: self.endpoint,
            endpoint_mode: self.endpoint_mode,
        }
    }
}

/// The table row whose configuration patch variant is `config_name`.
pub(crate) fn builtin_by_config_name(config_name: &str) -> Option<&'static BuiltinHttpProvider> {
    BUILTIN_HTTP_PROVIDERS
        .iter()
        .find(|row| row.config_name == config_name)
}

/// The table row for built-in provider `id`.
pub(crate) fn builtin_row(id: &str) -> &'static BuiltinHttpProvider {
    BUILTIN_HTTP_PROVIDERS
        .iter()
        .find(|row| row.id == id)
        .expect("a BuiltinProvider always names a table row")
}

/// The built-in HTTP providers, in the order credential variables are
/// published.
pub(crate) const BUILTIN_HTTP_PROVIDERS: [BuiltinHttpProvider; 5] = [
    BuiltinHttpProvider {
        id: "openai",
        config_name: "OpenAi",
        patch_fields: &["api_key", "models"],
        endpoint: OPENAI_ENDPOINT,
        endpoint_mode: EndpointMode::Exact,
        api: ProviderApi::OpenAiResponses,
        usage: UsageType::Metered,
        catalog: BuiltinCatalog::OpenAiApi,
        discovery: ModelDiscoveryShape::Supplementary,
        audience: BuiltinAudience::Table(OPENAI_CREDENTIAL_ENDPOINT),
        variables: &["OPENAI_API_KEY"],
        login: &[LoginMethod::ApiKey],
        init_model: "gpt-5.6",
        display_order: 0,
    },
    BuiltinHttpProvider {
        id: "openai-codex",
        config_name: "OpenAiCodex",
        patch_fields: &["profile", "models"],
        endpoint: CODEX_RESPONSES_ENDPOINT,
        endpoint_mode: EndpointMode::Exact,
        api: ProviderApi::OpenAiResponses,
        usage: UsageType::Subscription,
        catalog: BuiltinCatalog::OpenAiCodex,
        discovery: ModelDiscoveryShape::Codex,
        audience: BuiltinAudience::OpenAiCodex,
        variables: &[],
        login: &[
            LoginMethod::OpenAiCodexBrowser,
            LoginMethod::OpenAiCodexDevice,
        ],
        init_model: "gpt-5.6-luna",
        display_order: 4,
    },
    BuiltinHttpProvider {
        id: "anthropic",
        config_name: "Anthropic",
        patch_fields: &["api_key", "models"],
        endpoint: ANTHROPIC_ENDPOINT,
        endpoint_mode: EndpointMode::Exact,
        api: ProviderApi::AnthropicMessages,
        usage: UsageType::Metered,
        catalog: BuiltinCatalog::AnthropicApi,
        discovery: ModelDiscoveryShape::Authoritative,
        audience: BuiltinAudience::Table(ANTHROPIC_CREDENTIAL_ENDPOINT),
        variables: &["ANTHROPIC_API_KEY"],
        login: &[LoginMethod::ApiKey],
        init_model: "claude-sonnet-5",
        display_order: 1,
    },
    BuiltinHttpProvider {
        id: "google",
        config_name: "Google",
        patch_fields: &["api_key", "models"],
        endpoint: GOOGLE_ENDPOINT,
        endpoint_mode: EndpointMode::Base,
        api: ProviderApi::GoogleGenerateContent,
        usage: UsageType::Metered,
        catalog: BuiltinCatalog::GoogleApi,
        discovery: ModelDiscoveryShape::Supplementary,
        audience: BuiltinAudience::Table(GOOGLE_CREDENTIAL_ENDPOINT),
        variables: &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
        login: &[LoginMethod::ApiKey],
        init_model: "gemini-2.5-flash",
        display_order: 2,
    },
    BuiltinHttpProvider {
        id: "xai",
        config_name: "XAi",
        patch_fields: &["api_key", "profile", "models"],
        endpoint: XAI_ENDPOINT,
        endpoint_mode: EndpointMode::Base,
        api: ProviderApi::OpenAiChatCompletions,
        usage: UsageType::CredentialDependent,
        catalog: BuiltinCatalog::XAiApi,
        discovery: ModelDiscoveryShape::Supplementary,
        audience: BuiltinAudience::XAi,
        variables: &["XAI_API_KEY"],
        login: &[LoginMethod::ApiKey, LoginMethod::XAiOAuth],
        init_model: "grok-4.6",
        display_order: 3,
    },
];

/// Every environment variable a built-in provider reads for its credential
/// when no stored credential exists, in table order. The AWS default chain
/// reads the SDK's own variables; they are the SDK's contract, not QQ's, and
/// documented as a family.
pub fn provider_credential_variables() -> Vec<&'static str> {
    BUILTIN_HTTP_PROVIDERS
        .iter()
        .flat_map(|row| row.variables.iter().copied())
        .collect()
}

/// The built-in preset for `kind`, cloned from a table built once per process.
/// Every configuration load merges all seven presets, so building them (and
/// filtering the model table into each) per load was the largest fixed cost
/// of a load; a clone of the finished map is a fraction of that.
pub(crate) fn builtin(kind: ProviderKind) -> ProviderConfig {
    static HTTP: LazyLock<Vec<ProviderConfig>> =
        LazyLock::new(|| BUILTIN_HTTP_PROVIDERS.iter().map(build_http).collect());
    static BEDROCK: LazyLock<[ProviderConfig; 2]> = LazyLock::new(|| {
        [
            build_bedrock(ProviderKind::AmazonBedrock),
            build_bedrock(ProviderKind::AmazonBedrockMantle),
        ]
    });
    match kind {
        ProviderKind::Builtin(provider) => {
            let index = BUILTIN_HTTP_PROVIDERS
                .iter()
                .position(|row| row.id == provider.id)
                .expect("every built-in provider kind has a table row");
            HTTP[index].clone()
        }
        ProviderKind::AmazonBedrock => BEDROCK[0].clone(),
        ProviderKind::AmazonBedrockMantle => BEDROCK[1].clone(),
        ProviderKind::LiteLlm | ProviderKind::Custom => {
            ProviderConfig::new(kind, None, UsageType::Unknown, BTreeMap::new())
        }
    }
}

fn build_http(row: &BuiltinHttpProvider) -> ProviderConfig {
    ProviderConfig::new(
        ProviderKind::Builtin(row.public()),
        Some(http(
            row.endpoint,
            row.endpoint_mode,
            row.api,
            HttpCredential::Builtin {
                provider: row.public(),
                api_key: None,
                profile: None,
            },
        )),
        row.usage,
        builtin_models(row.catalog),
    )
}

fn build_bedrock(kind: ProviderKind) -> ProviderConfig {
    let (access, catalog) = match kind {
        ProviderKind::AmazonBedrock => (
            ProviderAccess::AmazonBedrock {
                region: None,
                auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
            },
            BuiltinCatalog::BedrockRuntime,
        ),
        ProviderKind::AmazonBedrockMantle => (
            ProviderAccess::AmazonBedrockMantle {
                region: None,
                api: ProviderApi::OpenAiResponses,
                auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
            },
            BuiltinCatalog::BedrockMantle,
        ),
        ProviderKind::Builtin(_) | ProviderKind::LiteLlm | ProviderKind::Custom => {
            unreachable!("{kind:?} is not a Bedrock preset")
        }
    };
    ProviderConfig::new(
        kind,
        Some(access),
        UsageType::Metered,
        builtin_models(catalog),
    )
}

fn http(
    endpoint: &str,
    endpoint_mode: EndpointMode,
    api: ProviderApi,
    auth: HttpCredential,
) -> ProviderAccess {
    ProviderAccess::Http(HttpAccess::new(
        endpoint,
        endpoint_mode,
        api,
        auth,
        BTreeMap::new(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every table row reaches the CLI and `qq init`, exactly once, in
    /// display order: adding a row is enough to list a provider.
    #[test]
    fn every_table_row_is_listed_once_in_display_order() {
        let listed: Vec<&str> = builtin_providers().map(|provider| provider.id).collect();
        let mut rows: Vec<&BuiltinHttpProvider> = BUILTIN_HTTP_PROVIDERS.iter().collect();
        rows.sort_by_key(|row| row.display_order);
        assert_eq!(listed, rows.iter().map(|row| row.id).collect::<Vec<_>>());
        assert_eq!(listed.len(), BUILTIN_HTTP_PROVIDERS.len());
        let mut orders: Vec<u8> = BUILTIN_HTTP_PROVIDERS
            .iter()
            .map(|row| row.display_order)
            .collect();
        orders.sort_unstable();
        orders.dedup();
        assert_eq!(
            orders.len(),
            BUILTIN_HTTP_PROVIDERS.len(),
            "display orders are unique"
        );
        assert_eq!(
            listed,
            ["openai", "anthropic", "google", "xai", "openai-codex"]
        );
    }

    /// An API-key row resolves its key from a variable when nothing is
    /// stored, and the plan descriptor names that variable, so every row
    /// whose audience the table owns lists at least one.
    #[test]
    fn every_api_key_row_names_a_variable() {
        for row in &BUILTIN_HTTP_PROVIDERS {
            if matches!(row.audience, BuiltinAudience::Table(_)) {
                assert!(!row.variables.is_empty(), "{} has no variable", row.id);
            }
            assert_eq!(builtin_provider(row.id).map(|p| p.id), Some(row.id));
        }
        assert_eq!(builtin_provider("bedrock"), None);
    }

    /// Configuration names are unique and every patch field is one the
    /// built-in patch parser knows, ending in `models`.
    #[test]
    fn every_row_has_a_unique_config_name_and_known_patch_fields() {
        for row in &BUILTIN_HTTP_PROVIDERS {
            assert_eq!(
                builtin_by_config_name(row.config_name).map(|found| found.id),
                Some(row.id)
            );
            assert_eq!(row.patch_fields.last(), Some(&"models"), "{}", row.id);
            for field in row.patch_fields {
                assert!(
                    ["api_key", "profile", "models"].contains(field),
                    "{}: {field}",
                    row.id
                );
            }
        }
    }
}
