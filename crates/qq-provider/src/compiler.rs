//! Provider recipe compilation.

use std::sync::Arc;

use crate::{
    Provider, ProviderError, SharedRequestCredentialProvider,
    bedrock_auth::BedrockAuth,
    construction::{HttpConstructionSpec, construct_http_provider},
    credentials::SecretLiteral,
    http::{AttemptPolicy, build_client, build_direct_client, validate_endpoint},
};
#[cfg(feature = "provider-bedrock")]
use crate::{
    construction::HttpRetryMode,
    providers::{bedrock::Bedrock, mantle::Mantle},
};

/// The configuration error a build without `provider-bedrock` returns for an
/// Amazon Bedrock recipe. Neutral request and event behavior is unchanged;
/// only compilation of that family is refused.
pub const BEDROCK_FAMILY_UNAVAILABLE_MESSAGE: &str = "this build of qq-provider was compiled without the provider-bedrock feature; Amazon Bedrock and Bedrock Mantle recipes cannot be compiled";

/// Compiles provider recipes while sharing expensive transport state.
#[derive(Clone)]
pub struct ProviderCompiler {
    http: reqwest::Client,
    direct_http: reqwest::Client,
    attempts: AttemptPolicy,
}

impl ProviderCompiler {
    /// Creates a compiler with one reusable HTTP connection pool.
    pub fn new() -> Result<Self, ProviderError> {
        Ok(Self {
            http: build_client()?,
            direct_http: build_direct_client()?,
            attempts: AttemptPolicy::default(),
        })
    }

    /// The attempt policy every HTTP provider this compiler produces carries.
    /// This is the only retry owner: nothing above the provider resends.
    #[must_use]
    pub fn with_attempt_policy(mut self, attempts: AttemptPolicy) -> Self {
        self.attempts = attempts;
        self
    }

    #[must_use]
    pub const fn attempt_policy(&self) -> AttemptPolicy {
        self.attempts
    }

    /// Validates and compiles one immutable provider recipe.
    pub fn compile(&self, recipe: ProviderRecipe) -> Result<Arc<dyn Provider>, ProviderError> {
        match recipe {
            ProviderRecipe::Http(recipe) => self.compile_http(recipe),
            #[cfg(feature = "provider-bedrock")]
            ProviderRecipe::AmazonBedrock { region, auth } => {
                Ok(Arc::new(Bedrock::new(auth, region)?))
            }
            #[cfg(feature = "provider-bedrock")]
            ProviderRecipe::AmazonBedrockMantle {
                region,
                protocol,
                auth,
            } => Ok(Arc::new(Mantle::new(
                self.direct_http.clone(),
                region,
                protocol,
                auth,
                HttpRetryMode::Default,
            )?)),
            #[cfg(not(feature = "provider-bedrock"))]
            ProviderRecipe::AmazonBedrock { .. } | ProviderRecipe::AmazonBedrockMantle { .. } => {
                Err(ProviderError::Configuration(
                    BEDROCK_FAMILY_UNAVAILABLE_MESSAGE.to_owned(),
                ))
            }
        }
    }

    /// Compiles one provider for an operational canary with adapter retries
    /// disabled, so the probe observes exactly one provider attempt.
    pub fn compile_for_canary(
        &self,
        recipe: ProviderRecipe,
    ) -> Result<Arc<dyn Provider>, ProviderError> {
        match recipe {
            ProviderRecipe::Http(recipe) => self.compile_http_for_canary(recipe),
            #[cfg(feature = "provider-bedrock")]
            ProviderRecipe::AmazonBedrock { region, auth } => {
                Ok(Arc::new(Bedrock::new(auth, region)?))
            }
            #[cfg(feature = "provider-bedrock")]
            ProviderRecipe::AmazonBedrockMantle {
                region,
                protocol,
                auth,
            } => Ok(Arc::new(Mantle::new(
                self.direct_http.clone(),
                region,
                protocol,
                auth,
                HttpRetryMode::Disabled,
            )?)),
            #[cfg(not(feature = "provider-bedrock"))]
            ProviderRecipe::AmazonBedrock { .. } | ProviderRecipe::AmazonBedrockMantle { .. } => {
                Err(ProviderError::Configuration(
                    BEDROCK_FAMILY_UNAVAILABLE_MESSAGE.to_owned(),
                ))
            }
        }
    }

    fn compile_http(&self, recipe: HttpProviderRecipe) -> Result<Arc<dyn Provider>, ProviderError> {
        Ok(Arc::new(
            self.construct_http(recipe)?
                .with_attempt_policy(self.attempts),
        ))
    }

    fn compile_http_for_canary(
        &self,
        recipe: HttpProviderRecipe,
    ) -> Result<Arc<dyn Provider>, ProviderError> {
        Ok(Arc::new(self.construct_http(recipe)?.without_retries()))
    }

    // Keep the ordinary compile path branch-free after adding the canary call site.
    #[inline(always)]
    fn construct_http(
        &self,
        recipe: HttpProviderRecipe,
    ) -> Result<crate::construction::CompiledHttpProvider, ProviderError> {
        let endpoint_kind = recipe.endpoint.kind;
        let endpoint = recipe.endpoint.resolve(recipe.protocol)?;
        let client = if endpoint.scheme() == "http" {
            self.direct_http.clone()
        } else {
            self.http.clone()
        };
        let spec = HttpConstructionSpec {
            protocol: recipe.protocol,
            endpoint,
            endpoint_kind,
            auth: recipe.auth,
            authorizer: None,
            headers: recipe.headers,
            body_shape: recipe.body_shape,
        };

        construct_http_provider(client, spec)
    }
}

/// An uncompiled provider deployment recipe.
pub enum ProviderRecipe {
    Http(HttpProviderRecipe),
    AmazonBedrock {
        region: Option<String>,
        auth: BedrockAuth,
    },
    AmazonBedrockMantle {
        region: Option<String>,
        protocol: HttpProtocol,
        auth: BedrockAuth,
    },
}

impl ProviderRecipe {
    #[must_use]
    pub fn http(recipe: HttpProviderRecipe) -> Self {
        Self::Http(recipe)
    }

    #[must_use]
    pub fn amazon_bedrock(region: Option<String>, auth: BedrockAuth) -> Self {
        Self::AmazonBedrock { region, auth }
    }

    #[must_use]
    pub fn amazon_bedrock_mantle(
        region: Option<String>,
        protocol: HttpProtocol,
        auth: BedrockAuth,
    ) -> Self {
        Self::AmazonBedrockMantle {
            region,
            protocol,
            auth,
        }
    }
}

/// A protocol-bound HTTP provider recipe.
pub struct HttpProviderRecipe {
    endpoint: EndpointSpec,
    protocol: HttpProtocol,
    auth: HttpAuth,
    headers: Vec<(String, String)>,
    body_shape: ResponsesBodyShape,
}

impl HttpProviderRecipe {
    /// A recipe whose Responses body shape is the one its auth intent
    /// implies: Codex for request-time Codex credentials, standard otherwise.
    /// Use [`Self::with_body_shape`] to choose it independently.
    #[must_use]
    pub fn new(endpoint: EndpointSpec, protocol: HttpProtocol, auth: HttpAuth) -> Self {
        let body_shape = ResponsesBodyShape::implied_by(&auth);
        Self {
            endpoint,
            protocol,
            auth,
            headers: Vec::new(),
            body_shape,
        }
    }

    #[must_use]
    pub fn with_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.headers = headers;
        self
    }

    /// Selects the Responses request body shape. Only OpenAI Responses has
    /// more than one; compiling another protocol with
    /// [`ResponsesBodyShape::Codex`] is a configuration error.
    #[must_use]
    pub fn with_body_shape(mut self, body_shape: ResponsesBodyShape) -> Self {
        self.body_shape = body_shape;
        self
    }
}

/// The OpenAI Responses request body a compiled provider sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResponsesBodyShape {
    /// The public Responses API body, including `max_output_tokens`.
    Standard,
    /// The ChatGPT Codex backend body, which omits `max_output_tokens` and
    /// may stream SSE without a `Content-Type`.
    Codex,
}

impl ResponsesBodyShape {
    /// The shape the auth intent implies before the shape is explicit.
    #[must_use]
    pub const fn implied_by(auth: &HttpAuth) -> Self {
        match auth {
            HttpAuth::RequestTimeCodex(_) => Self::Codex,
            HttpAuth::NoAuth
            | HttpAuth::ApiKey(_)
            | HttpAuth::Bearer(_)
            | HttpAuth::Header(..)
            | HttpAuth::RequestTimeBearer(_) => Self::Standard,
        }
    }
}

/// Whether a configured URL is a protocol-independent base or an exact endpoint.
pub struct EndpointSpec {
    url: String,
    kind: EndpointKind,
    allow_http: bool,
}

impl EndpointSpec {
    #[must_use]
    pub fn base(url: impl Into<String>, allow_http: bool) -> Self {
        Self {
            url: url.into(),
            kind: EndpointKind::Base,
            allow_http,
        }
    }

    #[must_use]
    pub fn exact(url: impl Into<String>, allow_http: bool) -> Self {
        Self {
            url: url.into(),
            kind: EndpointKind::Exact,
            allow_http,
        }
    }

    fn resolve(self, protocol: HttpProtocol) -> Result<reqwest::Url, ProviderError> {
        let mut url = validate_endpoint(&self.url, self.allow_http)?;
        if self.kind == EndpointKind::Exact {
            return Ok(url);
        }
        if url.query().is_some() {
            return Err(ProviderError::Configuration(
                "base endpoint URL must not contain a query".to_owned(),
            ));
        }

        url.path_segments_mut()
            .map_err(|()| {
                ProviderError::Configuration(
                    "base endpoint URL cannot contain protocol paths".to_owned(),
                )
            })?
            .pop_if_empty()
            .extend(protocol.path_segments());
        Ok(url)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EndpointKind {
    Base,
    Exact,
}

/// HTTP wire protocols supported by the compiler.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpProtocol {
    OpenAiResponses,
    OpenAiChatCompletions,
    AnthropicMessages,
    GoogleGenerateContent,
}

impl HttpProtocol {
    fn path_segments(self) -> &'static [&'static str] {
        match self {
            Self::OpenAiResponses => &["responses"],
            Self::OpenAiChatCompletions => &["chat", "completions"],
            Self::AnthropicMessages => &["messages"],
            Self::GoogleGenerateContent => &[],
        }
    }
}

/// Protocol-independent HTTP authentication intent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpAuth {
    NoAuth,
    ApiKey(SecretLiteral),
    Bearer(SecretLiteral),
    Header(String, SecretLiteral),
    /// Bearer credentials resolved on each request.
    RequestTimeBearer(SharedRequestCredentialProvider),
    /// Codex credentials resolved on each request.
    ///
    /// This intent is valid only for OpenAI Responses. [`HttpProviderRecipe::new`]
    /// pairs it with [`ResponsesBodyShape::Codex`] unless another shape is set.
    RequestTimeCodex(SharedRequestCredentialProvider),
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;

    use crate::{Message, ModelRequest, ProviderEvent, test_support::LoopbackServer};

    use super::*;

    #[tokio::test]
    async fn compiles_a_base_endpoint_into_a_streaming_provider() {
        let server = LoopbackServer::sse(concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"fast\"}\n\n",
            "data: {\"type\":\"response.completed\"}\n\n",
        ));
        let base_url = server.base_url.clone();
        let compiler = ProviderCompiler::new().unwrap();
        let provider = compiler
            .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                EndpointSpec::base(format!("{base_url}/v1"), true),
                HttpProtocol::OpenAiResponses,
                HttpAuth::ApiKey("test-secret".into()),
            )))
            .unwrap();

        let events = provider
            .stream(ModelRequest::new(
                "test-model",
                vec![Message::user("hello")],
                64,
            ))
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            &events[..],
            [
                Ok(ProviderEvent::OutputTextDelta { text }),
                Ok(ProviderEvent::Completed { usage: None })
            ] if text == "fast"
        ));
        let request = server.capture();
        assert_eq!(request.request_line(), Some("POST /v1/responses HTTP/1.1"));
        assert_eq!(request.header("authorization"), Some("Bearer test-secret"));
        assert_eq!(request.json_body()["max_output_tokens"], 64);
    }

    #[tokio::test]
    async fn request_time_codex_auth_omits_max_output_tokens() {
        let server = LoopbackServer::sse("data: {\"type\":\"response.completed\"}\n\n");
        let base_url = server.base_url.clone();
        let compiler = ProviderCompiler::new().unwrap();
        let provider = compiler
            .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                EndpointSpec::exact(format!("{base_url}/backend-api/codex/responses"), true),
                HttpProtocol::OpenAiResponses,
                HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                    StaticCodexCredentials,
                )),
            )))
            .unwrap();

        let events = provider
            .stream(ModelRequest::new(
                "gpt-test",
                vec![Message::user("ping")],
                128,
            ))
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::Completed { usage: None })
        ));
        let request = server.capture();
        assert_eq!(
            request.request_line(),
            Some("POST /backend-api/codex/responses HTTP/1.1")
        );
        assert_eq!(
            request.header("authorization"),
            Some("Bearer codex-test-access-token")
        );
        assert_eq!(
            request.header("chatgpt-account-id"),
            Some("workspace-test-id")
        );
        assert_eq!(request.header("originator"), Some("qq"));

        let request_body = request.json_body();
        assert_eq!(request_body["model"], "gpt-test");
        assert!(
            !request_body
                .as_object()
                .unwrap()
                .contains_key("max_output_tokens")
        );
    }

    struct MismatchedBearerCredentials;

    impl crate::RequestCredentialProvider for MismatchedBearerCredentials {
        fn credential(&self) -> crate::RequestCredentialFuture<'_> {
            Box::pin(async {
                crate::RequestCredential::codex("wrong-codex-token", "wrong-account", false)
            })
        }
    }

    struct MismatchedCodexCredentials;

    impl crate::RequestCredentialProvider for MismatchedCodexCredentials {
        fn credential(&self) -> crate::RequestCredentialFuture<'_> {
            Box::pin(async { crate::RequestCredential::bearer("wrong-bearer-token") })
        }
    }

    #[tokio::test]
    async fn rejects_request_time_credential_kind_mismatch_before_network_io() {
        let cases = [
            HttpAuth::RequestTimeBearer(SharedRequestCredentialProvider::new(
                MismatchedBearerCredentials,
            )),
            HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                MismatchedCodexCredentials,
            )),
        ];

        for auth in cases {
            let provider = ProviderCompiler::new()
                .unwrap()
                .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                    EndpointSpec::exact("http://127.0.0.1:1/v1/responses", true),
                    HttpProtocol::OpenAiResponses,
                    auth,
                )))
                .unwrap();
            let events = provider
                .stream(ModelRequest::new(
                    "gpt-test",
                    vec![Message::user("ping")],
                    64,
                ))
                .collect::<Vec<_>>()
                .await;

            assert!(matches!(
                events.as_slice(),
                [Err(ProviderError::Configuration(message))]
                    if message == "request credential kind did not match configured authorization intent"
            ));
        }
    }

    struct StaticBearerCredentials;

    impl crate::RequestCredentialProvider for StaticBearerCredentials {
        fn credential(&self) -> crate::RequestCredentialFuture<'_> {
            Box::pin(async { crate::RequestCredential::bearer("request-time-bearer-token") })
        }
    }

    #[tokio::test]
    async fn request_time_bearer_keeps_standard_request_shape() {
        let server = LoopbackServer::sse("data: {\"type\":\"response.completed\"}\n\n");
        let provider = ProviderCompiler::new()
            .unwrap()
            .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                EndpointSpec::base(format!("{}/v1", server.base_url), true),
                HttpProtocol::OpenAiResponses,
                HttpAuth::RequestTimeBearer(SharedRequestCredentialProvider::new(
                    StaticBearerCredentials,
                )),
            )))
            .unwrap();

        let events = provider
            .stream(ModelRequest::new(
                "gpt-test",
                vec![Message::user("ping")],
                128,
            ))
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::Completed { usage: None })
        ));
        let request = server.capture();
        assert_eq!(
            request.header("authorization"),
            Some("Bearer request-time-bearer-token")
        );
        assert_eq!(request.json_body()["max_output_tokens"], 128);
    }

    struct StaticCodexCredentials;

    impl crate::RequestCredentialProvider for StaticCodexCredentials {
        fn credential(&self) -> crate::RequestCredentialFuture<'_> {
            Box::pin(async {
                crate::RequestCredential::codex(
                    "codex-test-access-token",
                    "workspace-test-id",
                    false,
                )
            })
        }
    }

    #[tokio::test]
    async fn compiles_chat_anthropic_and_google_through_the_provider_interface() {
        let cases = [
            (
                HttpProtocol::OpenAiChatCompletions,
                "/v1",
                "data: {\"choices\":[]}\n\ndata: [DONE]\n\n",
                "POST /v1/chat/completions HTTP/1.1",
                "authorization",
                "chat-secret",
                "Bearer chat-secret",
            ),
            (
                HttpProtocol::AnthropicMessages,
                "/v1",
                concat!(
                    "event: message_start\n",
                    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
                    "event: message_stop\n",
                    "data: {\"type\":\"message_stop\"}\n\n",
                ),
                "POST /v1/messages HTTP/1.1",
                "x-api-key",
                "anthropic-secret",
                "anthropic-secret",
            ),
            (
                HttpProtocol::GoogleGenerateContent,
                "",
                "data: {\"candidates\":[{\"finishReason\":\"STOP\",\"index\":0}]}\n\n",
                "POST /models/test-model:streamGenerateContent?alt=sse HTTP/1.1",
                "x-goog-api-key",
                "google-secret",
                "google-secret",
            ),
        ];

        for (protocol, base_path, response, request_line, auth_header, api_key, expected_auth) in
            cases
        {
            let server = LoopbackServer::sse(response);
            let endpoint = format!("{}{base_path}", server.base_url);
            let provider = ProviderCompiler::new()
                .unwrap()
                .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                    EndpointSpec::base(endpoint, true),
                    protocol,
                    HttpAuth::ApiKey(api_key.into()),
                )))
                .unwrap();

            let events = provider
                .stream(ModelRequest::new(
                    "test-model",
                    vec![Message::user("hello")],
                    64,
                ))
                .collect::<Vec<_>>()
                .await;

            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, Ok(ProviderEvent::Completed { .. }))),
                "{protocol:?} did not complete: {events:?}"
            );
            let request = server.capture();
            assert_eq!(request.request_line(), Some(request_line));
            assert_eq!(request.header(auth_header), Some(expected_auth));
        }
    }

    #[test]
    fn rejects_request_time_codex_for_non_responses_protocols() {
        for protocol in [
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
            HttpProtocol::GoogleGenerateContent,
        ] {
            let error = ProviderCompiler::new()
                .unwrap()
                .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                    EndpointSpec::exact("https://example.test/provider", false),
                    protocol,
                    HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                        StaticCodexCredentials,
                    )),
                )))
                .err()
                .expect("request-time Codex must be restricted to Responses");

            assert!(matches!(error, ProviderError::Configuration(_)));
        }
    }

    #[test]
    fn endpoint_modes_are_explicit_and_segment_aware() {
        let base = EndpointSpec::base("https://example.test/myresponses", false)
            .resolve(HttpProtocol::OpenAiResponses)
            .unwrap();
        let exact = EndpointSpec::exact("https://example.test/myresponses", false)
            .resolve(HttpProtocol::OpenAiResponses)
            .unwrap();

        assert_eq!(base.as_str(), "https://example.test/myresponses/responses");
        assert_eq!(exact.as_str(), "https://example.test/myresponses");
    }

    #[test]
    fn rejects_invalid_recipe_combinations_before_network_io() {
        let compiler = ProviderCompiler::new().unwrap();
        let error = compiler
            .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                EndpointSpec::exact("https://example.test/v1/chat/completions", false),
                HttpProtocol::OpenAiChatCompletions,
                HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                    StaticCodexCredentials,
                )),
            )))
            .err()
            .expect("incompatible recipe must fail");

        assert!(matches!(error, ProviderError::Configuration(_)));
    }

    /// Both feature profiles construct and inspect Bedrock recipes; only
    /// compilation differs. Without `provider-bedrock` the compiler refuses
    /// the family with a configuration error before any network or SDK work,
    /// and the shared HTTP contract fixtures above are unaffected.
    #[test]
    fn bedrock_recipes_compile_only_with_the_provider_bedrock_feature() {
        let compiler = ProviderCompiler::new().unwrap();
        let recipes = [
            ProviderRecipe::amazon_bedrock(
                Some("us-east-1".to_owned()),
                BedrockAuth::ApiKey("test-key".into()),
            ),
            ProviderRecipe::amazon_bedrock_mantle(
                Some("us-east-1".to_owned()),
                HttpProtocol::OpenAiResponses,
                BedrockAuth::DefaultChain,
            ),
        ];
        for recipe in recipes {
            let compiled = compiler.compile(recipe);
            if cfg!(feature = "provider-bedrock") {
                assert!(compiled.is_ok(), "full profile compiles Bedrock recipes");
            } else {
                match compiled {
                    Err(ProviderError::Configuration(message)) => {
                        assert_eq!(message, BEDROCK_FAMILY_UNAVAILABLE_MESSAGE);
                    }
                    Err(other) => panic!("unexpected error kind: {other}"),
                    Ok(_) => panic!("minimal profile must refuse Bedrock recipes"),
                }
            }
        }
    }

    struct StaticBearer(&'static str);

    impl crate::RequestCredentialProvider for StaticBearer {
        fn credential(&self) -> crate::RequestCredentialFuture<'_> {
            let token = self.0;
            Box::pin(async move { crate::RequestCredential::bearer(token) })
        }
    }

    /// The body shape is the recipe's choice, not the credential's: a
    /// Codex-shaped body with plain bearer auth omits `max_output_tokens`, and
    /// a standard body with Codex request-time credentials includes it while
    /// still sending the Codex headers.
    #[tokio::test]
    async fn responses_body_shape_is_independent_of_the_auth_intent() {
        let cases = [
            (
                HttpAuth::RequestTimeBearer(SharedRequestCredentialProvider::new(StaticBearer(
                    "bearer-test-token",
                ))),
                ResponsesBodyShape::Codex,
                false,
                None,
            ),
            (
                HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                    StaticCodexCredentials,
                )),
                ResponsesBodyShape::Standard,
                true,
                Some("workspace-test-id"),
            ),
        ];
        for (auth, shape, has_max_output_tokens, account_id) in cases {
            let server = LoopbackServer::sse("data: {\"type\":\"response.completed\"}\n\n");
            let provider = ProviderCompiler::new()
                .unwrap()
                .compile_for_canary(ProviderRecipe::http(
                    HttpProviderRecipe::new(
                        EndpointSpec::exact(format!("{}/v1/responses", server.base_url), true),
                        HttpProtocol::OpenAiResponses,
                        auth,
                    )
                    .with_body_shape(shape),
                ))
                .unwrap();
            let events = provider
                .stream(ModelRequest::new(
                    "gpt-test",
                    vec![Message::user("ping")],
                    64,
                ))
                .collect::<Vec<_>>()
                .await;
            assert!(
                matches!(&events[..], [Ok(ProviderEvent::Completed { .. })]),
                "{shape:?}: {events:?}"
            );
            let request = server.capture();
            assert_eq!(
                request
                    .json_body()
                    .as_object()
                    .unwrap()
                    .contains_key("max_output_tokens"),
                has_max_output_tokens,
                "{shape:?}"
            );
            assert_eq!(
                request.header("chatgpt-account-id"),
                account_id,
                "{shape:?}"
            );
        }
    }

    #[test]
    fn the_codex_body_shape_is_rejected_outside_openai_responses() {
        for protocol in [
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
            HttpProtocol::GoogleGenerateContent,
        ] {
            let error = ProviderCompiler::new()
                .unwrap()
                .compile(ProviderRecipe::http(
                    HttpProviderRecipe::new(
                        EndpointSpec::exact("http://127.0.0.1:1/v1", true),
                        protocol,
                        HttpAuth::NoAuth,
                    )
                    .with_body_shape(ResponsesBodyShape::Codex),
                ))
                .err()
                .expect("Codex body shape needs Responses");
            assert!(
                matches!(&error, ProviderError::Configuration(message)
                    if message == "the Codex request body shape requires the OpenAI Responses protocol"),
                "{protocol:?}: {error:?}"
            );
        }
    }

    #[tokio::test]
    async fn google_accepts_request_time_bearer_credentials() {
        let server = LoopbackServer::sse(concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},",
            "\"finishReason\":\"STOP\"}]}\n\n"
        ));
        let provider = ProviderCompiler::new()
            .unwrap()
            .compile_for_canary(ProviderRecipe::http(HttpProviderRecipe::new(
                EndpointSpec::base(format!("{}/v1beta", server.base_url), true),
                HttpProtocol::GoogleGenerateContent,
                HttpAuth::RequestTimeBearer(SharedRequestCredentialProvider::new(StaticBearer(
                    "google-request-time-token",
                ))),
            )))
            .unwrap();
        let events = provider
            .stream(ModelRequest::new(
                "gemini-test",
                vec![Message::user("ping")],
                64,
            ))
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(Ok(ProviderEvent::Completed { .. }))),
            "{events:?}"
        );
        let request = server.capture();
        assert_eq!(
            request.header("authorization"),
            Some("Bearer google-request-time-token")
        );
        assert_eq!(request.header("x-goog-api-key"), None);
    }

    struct FedrampCodexCredentials;

    impl crate::RequestCredentialProvider for FedrampCodexCredentials {
        fn credential(&self) -> crate::RequestCredentialFuture<'_> {
            Box::pin(async {
                crate::RequestCredential::codex("echoed-codex-token", "echoed-codex-account", true)
            })
        }
    }

    /// A rejection body that echoes both Codex secrets reaches the caller
    /// with each one redacted, while the non-secret `true` metadata value,
    /// which also appears in ordinary JSON, survives intact.
    #[tokio::test]
    async fn request_time_codex_rejections_redact_every_secret_but_not_metadata() {
        let server = LoopbackServer::respond(
            401,
            "application/json",
            r#"{"error":{"message":"bad token echoed-codex-token for echoed-codex-account (fedramp=true)"}}"#,
        );
        let provider = ProviderCompiler::new()
            .unwrap()
            .compile_for_canary(ProviderRecipe::http(HttpProviderRecipe::new(
                EndpointSpec::exact(format!("{}/v1/responses", server.base_url), true),
                HttpProtocol::OpenAiResponses,
                HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                    FedrampCodexCredentials,
                )),
            )))
            .unwrap();
        let events = provider
            .stream(ModelRequest::new(
                "gpt-test",
                vec![Message::user("ping")],
                64,
            ))
            .collect::<Vec<_>>()
            .await;
        let request = server.capture();
        assert_eq!(request.header("x-openai-fedramp"), Some("true"));

        let [Err(error)] = events.as_slice() else {
            panic!("expected one rejection, got {events:?}");
        };
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains("echoed-codex-token"), "{rendered}");
        assert!(!rendered.contains("echoed-codex-account"), "{rendered}");
        assert!(
            rendered.contains("(fedramp=true)"),
            "metadata must not be redacted: {rendered}"
        );
    }

    /// Headers a request-time Codex credential owns cannot also be set as
    /// static headers: the duplicate is rejected at compile time instead of
    /// being overwritten or removed per attempt.
    #[test]
    fn request_time_codex_protects_its_headers_from_static_configuration() {
        for name in [
            "chatgpt-account-id",
            "originator",
            "x-openai-fedramp",
            "authorization",
        ] {
            let error = ProviderCompiler::new()
                .unwrap()
                .compile(ProviderRecipe::http(
                    HttpProviderRecipe::new(
                        EndpointSpec::exact("http://127.0.0.1:1/v1/responses", true),
                        HttpProtocol::OpenAiResponses,
                        HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
                            StaticCodexCredentials,
                        )),
                    )
                    .with_headers(vec![(name.to_owned(), "static-value".to_owned())]),
                ))
                .err()
                .unwrap_or_else(|| panic!("static `{name}` must be rejected"));
            assert!(
                matches!(&error, ProviderError::Configuration(message)
                    if message == &format!("static header `{name}` is controlled by the provider")),
                "{name}: {error:?}"
            );
        }
    }
}
