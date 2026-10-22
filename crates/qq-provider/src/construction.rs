//! Shared construction for compiled direct HTTP providers.
//!
//! This module owns protocol/authentication compatibility and concrete adapter
//! selection. Callers must resolve endpoints and supply an HTTP client before
//! entering this boundary.

use crate::{
    Provider, ProviderError, ProviderStream,
    compiler::{EndpointKind, HttpAuth, HttpProtocol, ResponsesBodyShape},
    http::AttemptPolicy,
    providers::{
        anthropic::{AnthropicAuth, AnthropicMessages},
        google::{GoogleAuth, GoogleGenerateContent},
        openai::{OpenAi, ResponsesAuth, ResponsesConstructionAuth},
        openai_chat::{ChatCompletionsAuth, OpenAiChatCompletions},
    },
    request_auth::RequestAuthorizer,
};

pub(crate) const GOOGLE_MANTLE_UNSUPPORTED_MESSAGE: &str =
    "Google GenerateContent is not supported by Amazon Bedrock Mantle";
const REQUEST_TIME_CODEX_REQUIRES_RESPONSES_MESSAGE: &str =
    "request-time Codex authentication requires the OpenAI Responses protocol";
const CODEX_BODY_REQUIRES_RESPONSES_MESSAGE: &str =
    "the Codex request body shape requires the OpenAI Responses protocol";
const AUTHORIZER_WITH_AUTH_MESSAGE: &str =
    "internal request authorization cannot be combined with HTTP authentication";

/// Fully resolved inputs for constructing one compiled HTTP provider.
pub(crate) struct HttpConstructionSpec {
    pub(crate) protocol: HttpProtocol,
    pub(crate) endpoint: reqwest::Url,
    pub(crate) endpoint_kind: EndpointKind,
    pub(crate) auth: HttpAuth,
    pub(crate) authorizer: Option<RequestAuthorizer>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body_shape: ResponsesBodyShape,
}

enum ResolvedHttpAuth {
    OpenAiResponses(ResponsesConstructionAuth),
    OpenAiChatCompletions {
        auth: ChatCompletionsAuth,
        authorizer: RequestAuthorizer,
    },
    AnthropicMessages {
        auth: AnthropicAuth,
        authorizer: RequestAuthorizer,
    },
    GoogleGenerateContent {
        auth: GoogleAuth,
        authorizer: RequestAuthorizer,
    },
}

// Only the Mantle adapter selects a retry mode at construction; the HTTP
// families take the canary path through `without_retries` directly.
#[cfg(feature = "provider-bedrock")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HttpRetryMode {
    Default,
    Disabled,
}

/// The concrete HTTP provider selected by compiled construction.
pub(crate) enum CompiledHttpProvider {
    OpenAiResponses(OpenAi),
    OpenAiChatCompletions(OpenAiChatCompletions),
    AnthropicMessages(AnthropicMessages),
    GoogleGenerateContent(GoogleGenerateContent),
}

impl CompiledHttpProvider {
    #[cfg(feature = "provider-bedrock")]
    #[must_use]
    pub(crate) fn with_retry_mode(self, retry_mode: HttpRetryMode) -> Self {
        if retry_mode == HttpRetryMode::Default {
            self
        } else {
            self.without_retries()
        }
    }

    #[must_use]
    pub(crate) fn without_retries(self) -> Self {
        self.with_attempt_policy(AttemptPolicy::disabled())
    }

    /// Replaces the single retry owner for every send this provider makes.
    #[must_use]
    pub(crate) fn with_attempt_policy(self, policy: AttemptPolicy) -> Self {
        let mut provider = self;
        let exchange = match &mut provider {
            Self::OpenAiResponses(provider) => &mut provider.exchange,
            Self::OpenAiChatCompletions(provider) => &mut provider.exchange,
            Self::AnthropicMessages(provider) => &mut provider.exchange,
            Self::GoogleGenerateContent(provider) => &mut provider.exchange,
        };
        exchange.set_attempt_policy(policy);
        provider
    }
}

impl Provider for CompiledHttpProvider {
    fn stream(&self, request: crate::ModelRequest) -> ProviderStream {
        match self {
            Self::OpenAiResponses(provider) => provider.stream(request),
            Self::OpenAiChatCompletions(provider) => provider.stream(request),
            Self::AnthropicMessages(provider) => provider.stream(request),
            Self::GoogleGenerateContent(provider) => provider.stream(request),
        }
    }
}

/// Constructs one HTTP adapter from resolved inputs without creating clients or
/// performing network I/O.
// Recipe compilation is a measured startup path with multiple facade call sites.
#[inline(always)]
pub(crate) fn construct_http_provider(
    client: reqwest::Client,
    spec: HttpConstructionSpec,
) -> Result<CompiledHttpProvider, ProviderError> {
    let HttpConstructionSpec {
        protocol,
        endpoint,
        endpoint_kind,
        auth,
        authorizer,
        headers,
        body_shape,
    } = spec;

    let resolved = resolve_http_auth(protocol, auth, authorizer)?;
    if body_shape == ResponsesBodyShape::Codex
        && !matches!(resolved, ResolvedHttpAuth::OpenAiResponses(_))
    {
        return Err(ProviderError::Configuration(
            CODEX_BODY_REQUIRES_RESPONSES_MESSAGE.to_owned(),
        ));
    }
    match resolved {
        ResolvedHttpAuth::OpenAiResponses(auth) => Ok(CompiledHttpProvider::OpenAiResponses(
            OpenAi::with_client_and_auth(client, endpoint, auth, headers, body_shape)?,
        )),
        ResolvedHttpAuth::OpenAiChatCompletions { auth, authorizer } => {
            Ok(CompiledHttpProvider::OpenAiChatCompletions(
                OpenAiChatCompletions::with_client_and_authorizer(
                    client, endpoint, auth, headers, authorizer,
                )?,
            ))
        }
        ResolvedHttpAuth::AnthropicMessages { auth, authorizer } => Ok(
            CompiledHttpProvider::AnthropicMessages(AnthropicMessages::with_client_and_authorizer(
                client, endpoint, auth, headers, authorizer,
            )?),
        ),
        ResolvedHttpAuth::GoogleGenerateContent { auth, authorizer } => Ok(
            CompiledHttpProvider::GoogleGenerateContent(GoogleGenerateContent::with_client(
                client,
                endpoint,
                endpoint_kind,
                auth,
                headers,
                authorizer,
            )?),
        ),
    }
}

fn resolve_http_auth(
    protocol: HttpProtocol,
    auth: HttpAuth,
    authorizer: Option<RequestAuthorizer>,
) -> Result<ResolvedHttpAuth, ProviderError> {
    match (auth, authorizer) {
        (HttpAuth::NoAuth, Some(authorizer)) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::MantleSigV4(authorizer),
            )),
            HttpProtocol::OpenAiChatCompletions => Ok(ResolvedHttpAuth::OpenAiChatCompletions {
                auth: ChatCompletionsAuth::NoAuth,
                authorizer,
            }),
            HttpProtocol::AnthropicMessages => Ok(ResolvedHttpAuth::AnthropicMessages {
                auth: AnthropicAuth::NoAuth,
                authorizer,
            }),
            HttpProtocol::GoogleGenerateContent => Err(ProviderError::Configuration(
                GOOGLE_MANTLE_UNSUPPORTED_MESSAGE.to_owned(),
            )),
        },
        (_, Some(_)) => Err(ProviderError::Configuration(
            AUTHORIZER_WITH_AUTH_MESSAGE.to_owned(),
        )),
        (HttpAuth::NoAuth, None) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::Static(ResponsesAuth::NoAuth),
            )),
            HttpProtocol::OpenAiChatCompletions => Ok(ResolvedHttpAuth::OpenAiChatCompletions {
                auth: ChatCompletionsAuth::NoAuth,
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::AnthropicMessages => Ok(ResolvedHttpAuth::AnthropicMessages {
                auth: AnthropicAuth::NoAuth,
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::GoogleGenerateContent => Ok(ResolvedHttpAuth::GoogleGenerateContent {
                auth: GoogleAuth::NoAuth,
                authorizer: RequestAuthorizer::default(),
            }),
        },
        (HttpAuth::ApiKey(secret), None) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::Static(ResponsesAuth::Bearer(secret)),
            )),
            HttpProtocol::OpenAiChatCompletions => Ok(ResolvedHttpAuth::OpenAiChatCompletions {
                auth: ChatCompletionsAuth::Bearer(secret),
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::AnthropicMessages => Ok(ResolvedHttpAuth::AnthropicMessages {
                auth: AnthropicAuth::XApiKey(secret),
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::GoogleGenerateContent => Ok(ResolvedHttpAuth::GoogleGenerateContent {
                auth: GoogleAuth::XGoogApiKey(secret),
                authorizer: RequestAuthorizer::default(),
            }),
        },
        (HttpAuth::Bearer(secret), None) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::Static(ResponsesAuth::Bearer(secret)),
            )),
            HttpProtocol::OpenAiChatCompletions => Ok(ResolvedHttpAuth::OpenAiChatCompletions {
                auth: ChatCompletionsAuth::Bearer(secret),
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::AnthropicMessages => Ok(ResolvedHttpAuth::AnthropicMessages {
                auth: AnthropicAuth::Bearer(secret),
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::GoogleGenerateContent => Ok(ResolvedHttpAuth::GoogleGenerateContent {
                auth: GoogleAuth::Bearer(secret),
                authorizer: RequestAuthorizer::default(),
            }),
        },
        (HttpAuth::Header(name, secret), None) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::Static(ResponsesAuth::Header(name, secret)),
            )),
            HttpProtocol::OpenAiChatCompletions => Ok(ResolvedHttpAuth::OpenAiChatCompletions {
                auth: ChatCompletionsAuth::Header(name, secret),
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::AnthropicMessages => Ok(ResolvedHttpAuth::AnthropicMessages {
                auth: AnthropicAuth::Header(name, secret),
                authorizer: RequestAuthorizer::default(),
            }),
            HttpProtocol::GoogleGenerateContent => Ok(ResolvedHttpAuth::GoogleGenerateContent {
                auth: GoogleAuth::Header(name, secret),
                authorizer: RequestAuthorizer::default(),
            }),
        },
        (HttpAuth::RequestTimeBearer(credentials), None) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::RequestTimeBearer(credentials),
            )),
            HttpProtocol::OpenAiChatCompletions => Ok(ResolvedHttpAuth::OpenAiChatCompletions {
                auth: ChatCompletionsAuth::NoAuth,
                authorizer: RequestAuthorizer::request_time_bearer(credentials),
            }),
            HttpProtocol::AnthropicMessages => Ok(ResolvedHttpAuth::AnthropicMessages {
                auth: AnthropicAuth::NoAuth,
                authorizer: RequestAuthorizer::request_time_bearer(credentials),
            }),
            HttpProtocol::GoogleGenerateContent => Ok(ResolvedHttpAuth::GoogleGenerateContent {
                auth: GoogleAuth::NoAuth,
                authorizer: RequestAuthorizer::request_time_bearer(credentials),
            }),
        },
        (HttpAuth::RequestTimeCodex(credentials), None) => match protocol {
            HttpProtocol::OpenAiResponses => Ok(ResolvedHttpAuth::OpenAiResponses(
                ResponsesConstructionAuth::RequestTimeCodex(credentials),
            )),
            _ => Err(ProviderError::Configuration(
                REQUEST_TIME_CODEX_REQUIRES_RESPONSES_MESSAGE.to_owned(),
            )),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        RequestCredential, RequestCredentialFuture, RequestCredentialProvider,
        SharedRequestCredentialProvider, compiler::HttpProtocol,
    };

    struct StaticBearerCredentials;

    impl RequestCredentialProvider for StaticBearerCredentials {
        fn credential(&self) -> RequestCredentialFuture<'_> {
            Box::pin(async { RequestCredential::bearer("test-bearer") })
        }
    }

    struct StaticCodexCredentials;

    impl RequestCredentialProvider for StaticCodexCredentials {
        fn credential(&self) -> RequestCredentialFuture<'_> {
            Box::pin(async { RequestCredential::codex("test-codex", "test-account", false) })
        }
    }

    fn spec(protocol: HttpProtocol, auth: HttpAuth) -> HttpConstructionSpec {
        HttpConstructionSpec {
            protocol,
            endpoint: reqwest::Url::parse("http://127.0.0.1:1/test").unwrap(),
            endpoint_kind: EndpointKind::Exact,
            body_shape: ResponsesBodyShape::Standard,
            auth,
            authorizer: None,
            headers: Vec::new(),
        }
    }

    #[test]
    fn constructs_every_protocol_without_network_io() {
        for protocol in [
            HttpProtocol::OpenAiResponses,
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
            HttpProtocol::GoogleGenerateContent,
        ] {
            construct_http_provider(
                reqwest::Client::new(),
                spec(protocol, HttpAuth::ApiKey("test-secret".into())),
            )
            .unwrap();
        }
    }

    #[test]
    fn constructs_every_supported_direct_auth_intent_without_network_io() {
        let cases = [
            HttpAuth::NoAuth,
            HttpAuth::ApiKey("test-api-key".into()),
            HttpAuth::Bearer("test-bearer".into()),
            HttpAuth::Header("x-test-key".to_owned(), "test-value".into()),
            HttpAuth::RequestTimeBearer(SharedRequestCredentialProvider::new(
                StaticBearerCredentials,
            )),
            HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                StaticCodexCredentials,
            )),
        ];

        for auth in cases {
            construct_http_provider(
                reqwest::Client::new(),
                HttpConstructionSpec {
                    protocol: HttpProtocol::OpenAiResponses,
                    endpoint: reqwest::Url::parse("http://127.0.0.1:1/test").unwrap(),
                    endpoint_kind: EndpointKind::Base,
                    body_shape: ResponsesBodyShape::implied_by(&auth),
                    auth,
                    authorizer: None,
                    headers: Vec::new(),
                },
            )
            .unwrap();
        }
    }

    #[test]
    fn rejects_codex_intent_for_non_responses_protocols() {
        for protocol in [
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
            HttpProtocol::GoogleGenerateContent,
        ] {
            let error = construct_http_provider(
                reqwest::Client::new(),
                spec(
                    protocol,
                    HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                        StaticCodexCredentials,
                    )),
                ),
            )
            .err()
            .expect("Codex intent must be rejected");

            assert!(matches!(error, ProviderError::Configuration(_)));
            let rendered = format!("{error:?} {error}");
            assert!(!rendered.contains("test-codex"));
            assert!(!rendered.contains("test-account"));
        }
    }

    #[test]
    fn accepts_mantle_sigv4_only_for_mantle_protocols() {
        for protocol in [
            HttpProtocol::OpenAiResponses,
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
        ] {
            let mut spec = spec(protocol, HttpAuth::NoAuth);
            spec.authorizer = Some(RequestAuthorizer::default());
            construct_http_provider(reqwest::Client::new(), spec).unwrap();
        }

        let mut spec = spec(HttpProtocol::GoogleGenerateContent, HttpAuth::NoAuth);
        spec.authorizer = Some(RequestAuthorizer::default());
        let error = construct_http_provider(reqwest::Client::new(), spec)
            .err()
            .expect("Google must reject Mantle SigV4");
        assert!(matches!(error, ProviderError::Configuration(_)));
    }

    #[test]
    fn rejects_internal_authorizer_combined_with_recipe_auth() {
        let error = construct_http_provider(
            reqwest::Client::new(),
            HttpConstructionSpec {
                protocol: HttpProtocol::OpenAiResponses,
                endpoint: reqwest::Url::parse("http://127.0.0.1:1/test").unwrap(),
                endpoint_kind: EndpointKind::Exact,
                auth: HttpAuth::ApiKey("test-secret".into()),
                authorizer: Some(RequestAuthorizer::default()),
                headers: Vec::new(),
                body_shape: ResponsesBodyShape::Standard,
            },
        )
        .err()
        .expect("recipe auth and an internal authorizer must be mutually exclusive");

        assert!(matches!(error, ProviderError::Configuration(_)));
    }
}
