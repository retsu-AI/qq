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

/// The built-in preset for `kind`, cloned from a table built once per process.
/// Every configuration load merges all seven presets, so building them (and
/// filtering the model table into each) per load was the largest fixed cost
/// of a load; a clone of the finished map is a fraction of that.
pub(crate) fn builtin(kind: ProviderKind) -> ProviderConfig {
    static BUILTINS: LazyLock<[ProviderConfig; 7]> = LazyLock::new(|| {
        [
            build(ProviderKind::OpenAi),
            build(ProviderKind::OpenAiCodex),
            build(ProviderKind::Anthropic),
            build(ProviderKind::Google),
            build(ProviderKind::XAi),
            build(ProviderKind::AmazonBedrock),
            build(ProviderKind::AmazonBedrockMantle),
        ]
    });
    match kind {
        ProviderKind::OpenAi => BUILTINS[0].clone(),
        ProviderKind::OpenAiCodex => BUILTINS[1].clone(),
        ProviderKind::Anthropic => BUILTINS[2].clone(),
        ProviderKind::Google => BUILTINS[3].clone(),
        ProviderKind::XAi => BUILTINS[4].clone(),
        ProviderKind::AmazonBedrock => BUILTINS[5].clone(),
        ProviderKind::AmazonBedrockMantle => BUILTINS[6].clone(),
        ProviderKind::LiteLlm | ProviderKind::Custom => build(kind),
    }
}

fn build(kind: ProviderKind) -> ProviderConfig {
    let (access, usage, catalog) = match kind {
        ProviderKind::OpenAi => (
            Some(http(
                OPENAI_ENDPOINT,
                EndpointMode::Exact,
                ProviderApi::OpenAiResponses,
                HttpCredential::ApiKey {
                    explicit: None,
                    stored_name: "openai/default",
                    environment_variable: "OPENAI_API_KEY",
                    audience: OPENAI_CREDENTIAL_ENDPOINT,
                },
            )),
            UsageType::Metered,
            BuiltinCatalog::OpenAiApi,
        ),
        ProviderKind::OpenAiCodex => (
            Some(http(
                CODEX_RESPONSES_ENDPOINT,
                EndpointMode::Exact,
                ProviderApi::OpenAiResponses,
                HttpCredential::OpenAiCodex { profile: None },
            )),
            UsageType::Subscription,
            BuiltinCatalog::OpenAiCodex,
        ),
        ProviderKind::Anthropic => (
            Some(http(
                ANTHROPIC_ENDPOINT,
                EndpointMode::Exact,
                ProviderApi::AnthropicMessages,
                HttpCredential::ApiKey {
                    explicit: None,
                    stored_name: "anthropic/default",
                    environment_variable: "ANTHROPIC_API_KEY",
                    audience: ANTHROPIC_CREDENTIAL_ENDPOINT,
                },
            )),
            UsageType::Metered,
            BuiltinCatalog::AnthropicApi,
        ),
        ProviderKind::Google => (
            Some(http(
                GOOGLE_ENDPOINT,
                EndpointMode::Base,
                ProviderApi::GoogleGenerateContent,
                HttpCredential::ApiKey {
                    explicit: None,
                    stored_name: "google/default",
                    environment_variable: "GEMINI_API_KEY",
                    audience: GOOGLE_CREDENTIAL_ENDPOINT,
                },
            )),
            UsageType::Metered,
            BuiltinCatalog::GoogleApi,
        ),
        ProviderKind::XAi => (
            Some(http(
                XAI_ENDPOINT,
                EndpointMode::Base,
                ProviderApi::OpenAiChatCompletions,
                HttpCredential::XAi {
                    api_key: None,
                    profile: None,
                },
            )),
            UsageType::CredentialDependent,
            BuiltinCatalog::XAiApi,
        ),
        ProviderKind::AmazonBedrock => (
            Some(ProviderAccess::AmazonBedrock {
                region: None,
                auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
            }),
            UsageType::Metered,
            BuiltinCatalog::BedrockRuntime,
        ),
        ProviderKind::AmazonBedrockMantle => (
            Some(ProviderAccess::AmazonBedrockMantle {
                region: None,
                api: ProviderApi::OpenAiResponses,
                auth: BedrockAuth::Aws(AwsAuth::DefaultChain),
            }),
            UsageType::Metered,
            BuiltinCatalog::BedrockMantle,
        ),
        ProviderKind::LiteLlm | ProviderKind::Custom => {
            return ProviderConfig::new(kind, None, UsageType::Unknown, BTreeMap::new());
        }
    };

    ProviderConfig::new(kind, access, usage, builtin_models(catalog))
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
