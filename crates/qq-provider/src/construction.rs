use std::borrow::Cow;

use crate::{
    Provider, ProviderError, ProviderStream,
    compiler::{CacheRouting, EndpointKind, HttpAuth, HttpProtocol, ResponsesBodyShape},
    credentials::StaticAuth,
    http::AttemptPolicy,
    providers::{
        anthropic::AnthropicMessages, google::GoogleGenerateContent, openai::OpenAi,
        openai_chat::OpenAiChatCompletions,
    },
    request_auth::RequestAuthorizer,
};

pub(crate) const GOOGLE_MANTLE_UNSUPPORTED_MESSAGE: &str =
    "Google GenerateContent is not supported by Amazon Bedrock Mantle";
const REQUEST_TIME_CODEX_REQUIRES_RESPONSES_MESSAGE: &str =
    "request-time Codex authentication requires the OpenAI Responses protocol";
const CODEX_BODY_REQUIRES_RESPONSES_MESSAGE: &str =
    "the Codex request body shape requires the OpenAI Responses protocol";
const CACHE_ROUTING_PROTOCOL_MESSAGE: &str =
    "the selected cache routing is not supported by this protocol";
const AUTHORIZER_WITH_AUTH_MESSAGE: &str =
    "internal request authorization cannot be combined with HTTP authentication";

pub(crate) struct HttpConstructionSpec {
    pub(crate) protocol: HttpProtocol,
    pub(crate) endpoint: reqwest::Url,
    pub(crate) endpoint_kind: EndpointKind,
    pub(crate) auth: HttpAuth,
    pub(crate) authorizer: Option<RequestAuthorizer>,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body_shape: ResponsesBodyShape,
    pub(crate) cache_routing: CacheRouting,
}

struct ResolvedHttpAuth {
    auth: StaticAuth,
    authorizer: RequestAuthorizer,
}

#[cfg(feature = "provider-bedrock")]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum HttpRetryMode {
    Default,
    Disabled,
}

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
        cache_routing,
    } = spec;

    if body_shape == ResponsesBodyShape::Codex && protocol != HttpProtocol::OpenAiResponses {
        return Err(ProviderError::Configuration(
            CODEX_BODY_REQUIRES_RESPONSES_MESSAGE.to_owned(),
        ));
    }
    match (cache_routing, protocol) {
        (CacheRouting::None, _)
        | (CacheRouting::PromptCacheKey, HttpProtocol::OpenAiResponses)
        | (CacheRouting::GrokConversation, HttpProtocol::OpenAiChatCompletions) => {}
        _ => {
            return Err(ProviderError::Configuration(
                CACHE_ROUTING_PROTOCOL_MESSAGE.to_owned(),
            ));
        }
    }
    let ResolvedHttpAuth { auth, authorizer } = resolve_http_auth(protocol, auth, authorizer)?;
    Ok(match protocol {
        HttpProtocol::OpenAiResponses => CompiledHttpProvider::OpenAiResponses(
            OpenAi::with_client(client, endpoint, auth, headers, authorizer, body_shape)?
                .with_prompt_cache_key(cache_routing == CacheRouting::PromptCacheKey),
        ),
        HttpProtocol::OpenAiChatCompletions => CompiledHttpProvider::OpenAiChatCompletions(
            OpenAiChatCompletions::with_client(client, endpoint, auth, headers, authorizer)?
                .with_grok_conversation(cache_routing == CacheRouting::GrokConversation),
        ),
        HttpProtocol::AnthropicMessages => CompiledHttpProvider::AnthropicMessages(
            AnthropicMessages::with_client(client, endpoint, auth, headers, authorizer)?,
        ),
        HttpProtocol::GoogleGenerateContent => {
            CompiledHttpProvider::GoogleGenerateContent(GoogleGenerateContent::with_client(
                client,
                endpoint,
                endpoint_kind,
                auth,
                headers,
                authorizer,
            )?)
        }
    })
}

fn api_key_auth(protocol: HttpProtocol, secret: crate::SecretLiteral) -> StaticAuth {
    match protocol {
        HttpProtocol::OpenAiResponses | HttpProtocol::OpenAiChatCompletions => {
            StaticAuth::Bearer(secret)
        }
        HttpProtocol::AnthropicMessages => StaticAuth::header("x-api-key", secret),
        HttpProtocol::GoogleGenerateContent => StaticAuth::header("x-goog-api-key", secret),
    }
}

fn resolve_http_auth(
    protocol: HttpProtocol,
    auth: HttpAuth,
    authorizer: Option<RequestAuthorizer>,
) -> Result<ResolvedHttpAuth, ProviderError> {
    let resolved = |auth, authorizer| Ok(ResolvedHttpAuth { auth, authorizer });
    match (auth, authorizer) {
        (HttpAuth::NoAuth, Some(_)) if protocol == HttpProtocol::GoogleGenerateContent => Err(
            ProviderError::Configuration(GOOGLE_MANTLE_UNSUPPORTED_MESSAGE.to_owned()),
        ),
        (HttpAuth::NoAuth, Some(authorizer)) => resolved(StaticAuth::None, authorizer),
        (_, Some(_)) => Err(ProviderError::Configuration(
            AUTHORIZER_WITH_AUTH_MESSAGE.to_owned(),
        )),
        (HttpAuth::NoAuth, None) => resolved(StaticAuth::None, RequestAuthorizer::default()),
        (HttpAuth::ApiKey(secret), None) => {
            resolved(api_key_auth(protocol, secret), RequestAuthorizer::default())
        }
        (HttpAuth::Bearer(secret), None) => {
            resolved(StaticAuth::Bearer(secret), RequestAuthorizer::default())
        }
        (HttpAuth::Header(name, secret), None) => resolved(
            StaticAuth::Header(Cow::Owned(name), secret),
            RequestAuthorizer::default(),
        ),
        (HttpAuth::RequestTimeBearer(credentials), None) => resolved(
            StaticAuth::None,
            RequestAuthorizer::request_time_bearer(credentials),
        ),
        (HttpAuth::RequestTimeCodex(credentials), None) => match protocol {
            HttpProtocol::OpenAiResponses => resolved(
                StaticAuth::None,
                RequestAuthorizer::request_time_codex(credentials),
            ),
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
            cache_routing: CacheRouting::None,
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
    fn cache_routing_is_accepted_only_on_its_own_protocol() {
        let protocols = [
            HttpProtocol::OpenAiResponses,
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
            HttpProtocol::GoogleGenerateContent,
        ];
        for (routing, accepted) in [
            (CacheRouting::PromptCacheKey, HttpProtocol::OpenAiResponses),
            (
                CacheRouting::GrokConversation,
                HttpProtocol::OpenAiChatCompletions,
            ),
        ] {
            for protocol in protocols {
                let mut spec = spec(protocol, HttpAuth::ApiKey("test-secret".into()));
                spec.cache_routing = routing;
                let result = construct_http_provider(reqwest::Client::new(), spec);
                if protocol == accepted {
                    assert!(result.is_ok(), "{routing:?} on {protocol:?}");
                } else {
                    assert!(
                        matches!(result, Err(ProviderError::Configuration(_))),
                        "{routing:?} on {protocol:?}"
                    );
                }
            }
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
                    cache_routing: CacheRouting::None,
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
                cache_routing: CacheRouting::None,
            },
        )
        .err()
        .expect("recipe auth and an internal authorizer must be mutually exclusive");

        assert!(matches!(error, ProviderError::Configuration(_)));
    }
}
