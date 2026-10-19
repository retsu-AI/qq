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

/// The API-key variable xAI reads when nothing is stored. xAI's credential is
/// a `HttpCredential::XAi` (OAuth or key) whose resolver reads this variable;
/// its table row names it so the published list stays complete.
pub const XAI_API_KEY_VARIABLE: &str = "XAI_API_KEY";

/// One built-in HTTP provider: everything that differs between them, as
/// data. Adding a preset adds a row here (ADR-0064); `builtin()`, the
/// credential-variable list and patch application read the table.
pub(crate) struct BuiltinHttpProvider {
    pub(crate) kind: ProviderKind,
    /// The provider id configuration documents use (`providers: {"<id>": ..}`).
    pub(crate) id: &'static str,
    endpoint: &'static str,
    endpoint_mode: EndpointMode,
    api: ProviderApi,
    usage: UsageType,
    catalog: BuiltinCatalog,
    pub(crate) credential: BuiltinCredential,
    /// How `qq auth login <id>` may obtain this provider's credential, in
    /// the order the CLI documents them; the first is the default.
    login: &'static [LoginMethod],
    /// The model `qq init` proposes for this provider.
    init_model: &'static str,
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

/// The public view of one built-in HTTP provider's row: what the CLI, `qq
/// init`, the doctor and credential resolution need, with no
/// configuration types.
#[derive(Clone, Copy, Debug)]
pub struct BuiltinProvider {
    pub id: &'static str,
    /// Where this provider's credential audience comes from.
    pub audience: BuiltinAudience,
    /// The variable read when nothing is stored, if any.
    pub environment_variable: Option<&'static str>,
    pub login: &'static [LoginMethod],
    pub init_model: &'static str,
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

/// The order `qq auth login`, its error message and `qq init` list the
/// built-in providers: API-key providers first, the subscription last.
const LOGIN_ORDER: [ProviderKind; 5] = [
    ProviderKind::OpenAi,
    ProviderKind::Anthropic,
    ProviderKind::Google,
    ProviderKind::XAi,
    ProviderKind::OpenAiCodex,
];

/// Every built-in HTTP provider, in the order the CLI lists them.
pub fn builtin_providers() -> impl Iterator<Item = BuiltinProvider> {
    LOGIN_ORDER
        .iter()
        .map(|kind| builtin_http(*kind).expect("every login provider has a table row"))
        .map(|row| BuiltinProvider {
            id: row.id,
            audience: match row.credential {
                BuiltinCredential::ApiKey { audience, .. } => BuiltinAudience::Table(audience),
                BuiltinCredential::OpenAiCodex => BuiltinAudience::OpenAiCodex,
                BuiltinCredential::XAi { .. } => BuiltinAudience::XAi,
            },
            environment_variable: row.credential.variables().next(),
            login: row.login,
            init_model: row.init_model,
        })
}

/// How a built-in HTTP provider is authenticated.
#[derive(Clone, Copy)]
pub(crate) enum BuiltinCredential {
    /// An API key: an explicit reference, else the stored `stored_name`,
    /// else the first set variable, bound to `audience`.
    ApiKey {
        stored_name: &'static str,
        environment_variable: &'static str,
        alternate_variables: &'static [&'static str],
        audience: &'static str,
    },
    /// The OpenAI Codex subscription credential for a profile.
    OpenAiCodex,
    /// An xAI API key or OAuth credential for a profile, with
    /// `environment_variable` as the API-key fallback.
    XAi { environment_variable: &'static str },
}

impl BuiltinCredential {
    fn preset(self) -> HttpCredential {
        match self {
            Self::ApiKey {
                stored_name,
                environment_variable,
                alternate_variables,
                audience,
            } => HttpCredential::ApiKey {
                explicit: None,
                stored_name,
                environment_variable,
                alternate_variables,
                audience,
            },
            Self::OpenAiCodex => HttpCredential::OpenAiCodex { profile: None },
            Self::XAi { .. } => HttpCredential::XAi {
                api_key: None,
                profile: None,
            },
        }
    }

    /// The environment variables this credential falls back to, in order.
    fn variables(self) -> impl Iterator<Item = &'static str> {
        let (first, rest): (Option<&'static str>, &'static [&'static str]) = match self {
            Self::ApiKey {
                environment_variable,
                alternate_variables,
                ..
            } => (Some(environment_variable), alternate_variables),
            Self::XAi {
                environment_variable,
            } => (Some(environment_variable), &[]),
            Self::OpenAiCodex => (None, &[]),
        };
        first.into_iter().chain(rest.iter().copied())
    }
}

/// The built-in HTTP providers, in the order credential variables are
/// published.
pub(crate) const BUILTIN_HTTP_PROVIDERS: [BuiltinHttpProvider; 5] = [
    BuiltinHttpProvider {
        kind: ProviderKind::OpenAi,
        id: "openai",
        endpoint: OPENAI_ENDPOINT,
        endpoint_mode: EndpointMode::Exact,
        api: ProviderApi::OpenAiResponses,
        usage: UsageType::Metered,
        catalog: BuiltinCatalog::OpenAiApi,
        credential: BuiltinCredential::ApiKey {
            stored_name: "openai/default",
            environment_variable: "OPENAI_API_KEY",
            alternate_variables: &[],
            audience: OPENAI_CREDENTIAL_ENDPOINT,
        },
        login: &[LoginMethod::ApiKey],
        init_model: "gpt-5.6",
    },
    BuiltinHttpProvider {
        kind: ProviderKind::OpenAiCodex,
        id: "openai-codex",
        endpoint: CODEX_RESPONSES_ENDPOINT,
        endpoint_mode: EndpointMode::Exact,
        api: ProviderApi::OpenAiResponses,
        usage: UsageType::Subscription,
        catalog: BuiltinCatalog::OpenAiCodex,
        credential: BuiltinCredential::OpenAiCodex,
        login: &[
            LoginMethod::OpenAiCodexBrowser,
            LoginMethod::OpenAiCodexDevice,
        ],
        init_model: "gpt-5.6-luna",
    },
    BuiltinHttpProvider {
        kind: ProviderKind::Anthropic,
        id: "anthropic",
        endpoint: ANTHROPIC_ENDPOINT,
        endpoint_mode: EndpointMode::Exact,
        api: ProviderApi::AnthropicMessages,
        usage: UsageType::Metered,
        catalog: BuiltinCatalog::AnthropicApi,
        credential: BuiltinCredential::ApiKey {
            stored_name: "anthropic/default",
            environment_variable: "ANTHROPIC_API_KEY",
            alternate_variables: &[],
            audience: ANTHROPIC_CREDENTIAL_ENDPOINT,
        },
        login: &[LoginMethod::ApiKey],
        init_model: "claude-sonnet-5",
    },
    BuiltinHttpProvider {
        kind: ProviderKind::Google,
        id: "google",
        endpoint: GOOGLE_ENDPOINT,
        endpoint_mode: EndpointMode::Base,
        api: ProviderApi::GoogleGenerateContent,
        usage: UsageType::Metered,
        catalog: BuiltinCatalog::GoogleApi,
        credential: BuiltinCredential::ApiKey {
            stored_name: "google/default",
            environment_variable: "GEMINI_API_KEY",
            alternate_variables: &["GOOGLE_API_KEY"],
            audience: GOOGLE_CREDENTIAL_ENDPOINT,
        },
        login: &[LoginMethod::ApiKey],
        init_model: "gemini-2.5-flash",
    },
    BuiltinHttpProvider {
        kind: ProviderKind::XAi,
        id: "xai",
        endpoint: XAI_ENDPOINT,
        endpoint_mode: EndpointMode::Base,
        api: ProviderApi::OpenAiChatCompletions,
        usage: UsageType::CredentialDependent,
        catalog: BuiltinCatalog::XAiApi,
        credential: BuiltinCredential::XAi {
            environment_variable: XAI_API_KEY_VARIABLE,
        },
        login: &[LoginMethod::ApiKey, LoginMethod::XAiOAuth],
        init_model: "grok-4.6",
    },
];

/// The table row for a built-in HTTP provider kind.
pub(crate) fn builtin_http(kind: ProviderKind) -> Option<&'static BuiltinHttpProvider> {
    BUILTIN_HTTP_PROVIDERS.iter().find(|row| row.kind == kind)
}

/// Every environment variable a built-in provider reads for its credential
/// when no stored credential exists, in table order. The AWS default chain
/// reads the SDK's own variables; they are the SDK's contract, not QQ's, and
/// documented as a family.
pub fn provider_credential_variables() -> Vec<&'static str> {
    BUILTIN_HTTP_PROVIDERS
        .iter()
        .flat_map(|row| row.credential.variables())
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
        ProviderKind::OpenAi
        | ProviderKind::OpenAiCodex
        | ProviderKind::Anthropic
        | ProviderKind::Google
        | ProviderKind::XAi => {
            let index = BUILTIN_HTTP_PROVIDERS
                .iter()
                .position(|row| row.kind == kind)
                .expect("every HTTP provider kind has a table row");
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
        row.kind,
        Some(http(
            row.endpoint,
            row.endpoint_mode,
            row.api,
            row.credential.preset(),
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
        ProviderKind::OpenAi
        | ProviderKind::OpenAiCodex
        | ProviderKind::Anthropic
        | ProviderKind::Google
        | ProviderKind::XAi
        | ProviderKind::LiteLlm
        | ProviderKind::Custom => unreachable!("{kind:?} is not a Bedrock preset"),
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
