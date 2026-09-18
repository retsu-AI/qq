//! Amazon Bedrock Mantle adapter for OpenAI- and Anthropic-compatible APIs.

use std::{fmt, sync::Arc};

use async_stream::try_stream;
use aws_config::Region;
use futures_util::StreamExt;
use tokio::sync::OnceCell;

use crate::{
    Provider, ProviderError, ProviderErrorKind, ProviderStream,
    aws::{
        AwsConfigLoadError, INVALID_REGION_MESSAGE, load_aws_config, valid_region_label,
        validate_configuration,
    },
    bedrock_auth::BedrockAuth,
    compiler::{EndpointKind, HttpAuth, HttpProtocol},
    construction::{
        CompiledHttpProvider, GOOGLE_MANTLE_UNSUPPORTED_MESSAGE, HttpConstructionSpec,
        HttpRetryMode, construct_http_provider,
    },
    http::validate_endpoint,
    request_auth::RequestAuthorizer,
    sanitize::sanitize_message,
};

/// A lazily initialized Amazon Bedrock Mantle deployment.
pub(crate) struct Mantle {
    inner: Arc<MantleInner>,
}

struct MantleInner {
    client: reqwest::Client,
    region: Option<String>,
    protocol: HttpProtocol,
    auth: BedrockAuth,
    retry_mode: HttpRetryMode,
    provider: OnceCell<CompiledHttpProvider>,
    #[cfg(test)]
    warm_streams: std::sync::atomic::AtomicUsize,
}

impl fmt::Debug for Mantle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Mantle")
            .field("region", &self.inner.region)
            .field("protocol", &self.inner.protocol)
            .field("auth", &self.inner.auth)
            .finish_non_exhaustive()
    }
}

impl Mantle {
    pub(crate) fn new(
        client: reqwest::Client,
        region: Option<String>,
        protocol: HttpProtocol,
        auth: BedrockAuth,
        retry_mode: HttpRetryMode,
    ) -> Result<Self, ProviderError> {
        if protocol == HttpProtocol::GoogleGenerateContent {
            return Err(ProviderError::Configuration(
                GOOGLE_MANTLE_UNSUPPORTED_MESSAGE.to_owned(),
            ));
        }
        validate_configuration(&auth, region.as_deref())?;
        let provider = match (region.as_deref(), &auth) {
            (Some(region), BedrockAuth::ApiKey(api_key)) => {
                let endpoint =
                    mantle_endpoint(region, protocol).map_err(|error| error.to_provider_error())?;
                let provider = construct_mantle_provider(
                    client.clone(),
                    endpoint,
                    protocol,
                    HttpAuth::ApiKey(api_key.clone()),
                    None,
                    retry_mode,
                )
                .map_err(|error| error.to_provider_error())?;
                OnceCell::from(provider)
            }
            (Some(region), BedrockAuth::DefaultChain | BedrockAuth::Profile(_)) => {
                mantle_endpoint(region, protocol).map_err(|error| error.to_provider_error())?;
                OnceCell::new()
            }
            (None, _) => OnceCell::new(),
        };
        Ok(Self {
            inner: Arc::new(MantleInner {
                client,
                region,
                protocol,
                auth,
                retry_mode,
                provider,
                #[cfg(test)]
                warm_streams: std::sync::atomic::AtomicUsize::new(0),
            }),
        })
    }
}

impl Provider for Mantle {
    fn stream(&self, request: crate::ModelRequest) -> ProviderStream {
        if let Some(error) = request.unsupported_reasoning_effort("Mantle") {
            return Box::pin(async_stream::stream! { yield Err(error); });
        }
        if let Some(provider) = self.inner.provider.get() {
            #[cfg(test)]
            self.inner
                .warm_streams
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return provider.stream(request);
        }

        let inner = Arc::clone(&self.inner);
        Box::pin(try_stream! {
            let provider = match inner.provider.get_or_try_init(|| inner.load_provider()).await {
                Ok(provider) => provider,
                Err(error) => Err(error.to_provider_error())?,
            };
            let mut events = provider.stream(request);
            while let Some(event) = events.next().await {
                yield event?;
            }
        })
    }
}

impl MantleInner {
    async fn load_provider(&self) -> Result<CompiledHttpProvider, MantleInitError> {
        let config = load_aws_config(&self.auth, self.region.as_deref()).await?;
        let region = config
            .sdk_config
            .region()
            .map(Region::as_ref)
            .ok_or_else(|| {
                MantleInitError::Configuration(
                    "AWS region provider chain did not resolve a region".to_owned(),
                )
            })?;
        let endpoint = mantle_endpoint(region, self.protocol)?;

        match &self.auth {
            BedrockAuth::ApiKey(api_key) => construct_mantle_provider(
                self.client.clone(),
                endpoint,
                self.protocol,
                HttpAuth::ApiKey(api_key.clone()),
                None,
                self.retry_mode,
            ),
            BedrockAuth::DefaultChain | BedrockAuth::Profile(_) => {
                let credentials = config.credentials.ok_or_else(|| {
                    MantleInitError::Authentication(
                        "AWS credential lease is unavailable".to_owned(),
                    )
                })?;
                construct_mantle_provider(
                    self.client.clone(),
                    endpoint,
                    self.protocol,
                    HttpAuth::NoAuth,
                    Some(RequestAuthorizer::bedrock_mantle_sigv4(region, credentials)),
                    self.retry_mode,
                )
            }
        }
    }
}

fn construct_mantle_provider(
    client: reqwest::Client,
    endpoint: reqwest::Url,
    protocol: HttpProtocol,
    auth: HttpAuth,
    authorizer: Option<RequestAuthorizer>,
    retry_mode: HttpRetryMode,
) -> Result<CompiledHttpProvider, MantleInitError> {
    construct_http_provider(
        client,
        HttpConstructionSpec {
            protocol,
            endpoint,
            endpoint_kind: EndpointKind::Exact,
            auth,
            authorizer,
            headers: Vec::new(),
        },
    )
    .map(|provider| provider.with_retry_mode(retry_mode))
    .map_err(MantleInitError::from)
}

fn mantle_endpoint(region: &str, protocol: HttpProtocol) -> Result<reqwest::Url, MantleInitError> {
    if !valid_region_label(region) {
        return Err(MantleInitError::Configuration(
            INVALID_REGION_MESSAGE.to_owned(),
        ));
    }
    let path = match protocol {
        HttpProtocol::OpenAiResponses => "/v1/responses",
        HttpProtocol::OpenAiChatCompletions => "/v1/chat/completions",
        HttpProtocol::AnthropicMessages => "/anthropic/v1/messages",
        HttpProtocol::GoogleGenerateContent => {
            return Err(MantleInitError::Configuration(
                GOOGLE_MANTLE_UNSUPPORTED_MESSAGE.to_owned(),
            ));
        }
    };
    validate_endpoint(
        &format!("https://bedrock-mantle.{region}.api.aws{path}"),
        false,
    )
    .map_err(MantleInitError::from)
}

#[derive(Debug)]
enum MantleInitError {
    Configuration(String),
    Authentication(String),
    AwsConfiguration(AwsConfigLoadError),
}

impl MantleInitError {
    fn to_provider_error(&self) -> ProviderError {
        match self {
            Self::Configuration(message) => {
                ProviderError::Configuration(sanitize_message(message, &[]))
            }
            Self::Authentication(message) => ProviderError::ResponseFailed {
                kind: ProviderErrorKind::Authentication,
                message: sanitize_message(message, &[]),
            },
            Self::AwsConfiguration(error) => error.to_provider_error(),
        }
    }
}

impl From<AwsConfigLoadError> for MantleInitError {
    fn from(error: AwsConfigLoadError) -> Self {
        Self::AwsConfiguration(error)
    }
}

impl From<ProviderError> for MantleInitError {
    fn from(error: ProviderError) -> Self {
        match error {
            ProviderError::Configuration(message) => Self::Configuration(message),
            error => Self::Configuration(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::{Duration, SystemTime},
    };

    use aws_credential_types::{
        Credentials,
        provider::{self, ProvideCredentials, SharedCredentialsProvider, future},
    };

    use super::*;
    use crate::{
        Message, ModelRequest, ProviderEvent, ProviderUsage, http::build_direct_client,
        test_support::LoopbackServer,
    };

    #[test]
    fn uses_canonical_regional_endpoints_for_each_protocol() {
        for (protocol, expected) in [
            (
                HttpProtocol::OpenAiResponses,
                "https://bedrock-mantle.us-east-1.api.aws/v1/responses",
            ),
            (
                HttpProtocol::OpenAiChatCompletions,
                "https://bedrock-mantle.us-east-1.api.aws/v1/chat/completions",
            ),
            (
                HttpProtocol::AnthropicMessages,
                "https://bedrock-mantle.us-east-1.api.aws/anthropic/v1/messages",
            ),
        ] {
            assert_eq!(
                mantle_endpoint("us-east-1", protocol).unwrap().as_str(),
                expected
            );
        }
        assert!(mantle_endpoint("us-east-1/path", HttpProtocol::OpenAiResponses).is_err());
        assert!(mantle_endpoint(&"a".repeat(64), HttpProtocol::OpenAiResponses).is_err());
    }

    #[test]
    fn rejects_google_mantle_before_initialization() {
        let error = Mantle::new(
            reqwest::Client::new(),
            Some("us-east-1".to_owned()),
            HttpProtocol::GoogleGenerateContent,
            BedrockAuth::ApiKey("mantle-test-secret".into()),
            HttpRetryMode::Default,
        )
        .expect_err("Google must be rejected before Mantle initialization");

        assert!(matches!(
            error,
            ProviderError::Configuration(message)
                if message == GOOGLE_MANTLE_UNSUPPORTED_MESSAGE
        ));
    }

    #[test]
    fn construction_is_network_free_and_redacts_api_keys() {
        let provider = Mantle::new(
            reqwest::Client::new(),
            Some("us-east-1".to_owned()),
            HttpProtocol::OpenAiResponses,
            BedrockAuth::ApiKey("mantle-test-secret".into()),
            HttpRetryMode::Default,
        )
        .unwrap();

        assert!(!format!("{provider:?}").contains("mantle-test-secret"));
        assert!(
            Mantle::new(
                reqwest::Client::new(),
                Some("us-east-1/path".to_owned()),
                HttpProtocol::OpenAiResponses,
                BedrockAuth::DefaultChain,
                HttpRetryMode::Default,
            )
            .is_err()
        );
    }

    #[test]
    fn initialized_provider_selects_the_synchronous_warm_stream_path() {
        let provider = Mantle::new(
            reqwest::Client::new(),
            Some("us-east-1".to_owned()),
            HttpProtocol::OpenAiResponses,
            BedrockAuth::ApiKey("mantle-test-secret".into()),
            HttpRetryMode::Default,
        )
        .unwrap();

        let stream = provider.stream(ModelRequest::new(
            "test-model",
            vec![Message::user("hello")],
            64,
        ));

        assert_eq!(provider.inner.warm_streams.load(Ordering::Relaxed), 1);
        drop(stream);
    }

    #[tokio::test]
    async fn failed_initialization_is_retryable() {
        let provider = OnceCell::new();
        let first = provider
            .get_or_try_init(|| async { Err::<CompiledHttpProvider, _>("temporary failure") })
            .await;
        assert_eq!(first.err(), Some("temporary failure"));

        let second = provider
            .get_or_try_init(|| async {
                construct_mantle_provider(
                    build_direct_client().unwrap(),
                    mantle_endpoint("us-east-1", HttpProtocol::OpenAiResponses).unwrap(),
                    HttpProtocol::OpenAiResponses,
                    HttpAuth::ApiKey("mantle-test-secret".into()),
                    None,
                    HttpRetryMode::Default,
                )
            })
            .await;

        assert!(second.is_ok());
        assert!(provider.get().is_some());
    }

    #[tokio::test]
    async fn rejects_reasoning_effort_before_aws_provider_initialization() {
        for protocol in [
            HttpProtocol::OpenAiResponses,
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
        ] {
            let provider = Mantle::new(
                reqwest::Client::new(),
                Some("us-east-1".to_owned()),
                protocol,
                BedrockAuth::DefaultChain,
                HttpRetryMode::Default,
            )
            .unwrap();
            assert!(provider.inner.provider.get().is_none());

            let events = provider
                .stream(
                    ModelRequest::new("test-model", vec![Message::user("hello")], 64)
                        .with_reasoning_effort(crate::ReasoningEffort::Medium),
                )
                .collect::<Vec<_>>()
                .await;

            assert!(matches!(
                events.as_slice(),
                [Err(ProviderError::Configuration(message))] if message.contains("Mantle")
            ));
            assert!(provider.inner.provider.get().is_none());
        }
    }

    #[tokio::test]
    async fn api_keys_use_protocol_specific_headers() {
        for protocol in [
            HttpProtocol::OpenAiResponses,
            HttpProtocol::OpenAiChatCompletions,
            HttpProtocol::AnthropicMessages,
        ] {
            let server = LoopbackServer::respond(401, "application/json", "{}");
            let endpoint = reqwest::Url::parse(&format!("{}/invoke", server.base_url)).unwrap();
            let provider = construct_mantle_provider(
                build_direct_client().unwrap(),
                endpoint,
                protocol,
                HttpAuth::ApiKey("mantle-test-secret".into()),
                None,
                HttpRetryMode::Default,
            )
            .unwrap();

            let _events = provider
                .stream(ModelRequest::new(
                    "test-model",
                    vec![Message::user("hello")],
                    64,
                ))
                .collect::<Vec<_>>()
                .await;
            let request = server.capture();

            match protocol {
                HttpProtocol::OpenAiResponses | HttpProtocol::OpenAiChatCompletions => {
                    assert_eq!(
                        request.header("authorization"),
                        Some("Bearer mantle-test-secret")
                    );
                    assert_eq!(request.header("x-api-key"), None);
                }
                HttpProtocol::AnthropicMessages => {
                    assert_eq!(request.header("authorization"), None);
                    assert_eq!(request.header("x-api-key"), Some("mantle-test-secret"));
                    assert_eq!(request.header("anthropic-version"), Some("2023-06-01"));
                }
                HttpProtocol::GoogleGenerateContent => unreachable!("test cases are exhaustive"),
            }
        }
    }

    #[tokio::test]
    async fn delegates_usage_capture_to_each_protocol_codec() {
        let cases = [
            (
                HttpProtocol::OpenAiResponses,
                "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":17,\"input_tokens_details\":{\"cached_tokens\":5},\"output_tokens\":9}}}\n\n",
                ProviderUsage {
                    input_tokens: 12,
                    cache_read_input_tokens: 5,
                    cache_write_input_tokens: 0,
                    output_tokens: 9,
                    reasoning_tokens: None,
                },
            ),
            (
                HttpProtocol::OpenAiChatCompletions,
                concat!(
                    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":17,\"completion_tokens\":9,",
                    "\"prompt_tokens_details\":{\"cached_tokens\":5}}}\n\n",
                    "data: [DONE]\n\n",
                ),
                ProviderUsage {
                    input_tokens: 12,
                    cache_read_input_tokens: 5,
                    cache_write_input_tokens: 0,
                    output_tokens: 9,
                    reasoning_tokens: None,
                },
            ),
            (
                HttpProtocol::AnthropicMessages,
                concat!(
                    "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{",
                    "\"input_tokens\":12,\"cache_creation_input_tokens\":3,\"cache_read_input_tokens\":5,\"output_tokens\":1}}}\n\n",
                    "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":9}}\n\n",
                    "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
                ),
                ProviderUsage {
                    input_tokens: 12,
                    cache_read_input_tokens: 5,
                    cache_write_input_tokens: 3,
                    output_tokens: 9,
                    reasoning_tokens: None,
                },
            ),
        ];

        for (protocol, body, expected) in cases {
            let server = LoopbackServer::sse(body);
            let endpoint = reqwest::Url::parse(&format!("{}/invoke", server.base_url)).unwrap();
            let provider = construct_mantle_provider(
                build_direct_client().unwrap(),
                endpoint,
                protocol,
                HttpAuth::ApiKey("mantle-test-secret".into()),
                None,
                HttpRetryMode::Default,
            )
            .unwrap();
            let events = provider
                .stream(ModelRequest::new(
                    "test-model",
                    vec![Message::user("hello")],
                    64,
                ))
                .collect::<Vec<_>>()
                .await;
            server.capture();

            assert!(matches!(
                &events[..],
                [Ok(ProviderEvent::Completed { usage: Some(usage) })] if *usage == expected
            ));
        }
    }

    #[tokio::test]
    async fn codec_applies_sigv4_after_serializing_the_request() {
        let calls = Arc::new(AtomicUsize::new(0));
        let credentials = CountingCredentials {
            calls: Arc::clone(&calls),
            credentials: Credentials::new(
                "AKIDEXAMPLE",
                "test-secret-access-key",
                None,
                None,
                "test",
            ),
        };
        let authorizer =
            RequestAuthorizer::with_sigv4_for_test(crate::aws::SigV4Authorizer::with_clock(
                "us-east-1",
                SharedCredentialsProvider::new(credentials),
                fixed_time,
            ));
        let server = LoopbackServer::respond(401, "application/json", "{}");
        let endpoint = reqwest::Url::parse(&format!("{}/invoke", server.base_url)).unwrap();
        let provider = construct_mantle_provider(
            build_direct_client().unwrap(),
            endpoint,
            HttpProtocol::OpenAiResponses,
            HttpAuth::NoAuth,
            Some(authorizer),
            HttpRetryMode::Default,
        )
        .unwrap();

        let _events = provider
            .stream(ModelRequest::new(
                "test-model",
                vec![Message::user("hello")],
                64,
            ))
            .collect::<Vec<_>>()
            .await;
        let request = server.capture();
        let authorization = request.header("authorization").unwrap();
        let body = request.json_body();

        assert!(authorization.contains("/us-east-1/bedrock-mantle/aws4_request"));
        assert_eq!(body["model"], "test-model");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    fn fixed_time() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_767_225_600)
    }

    #[derive(Debug)]
    struct CountingCredentials {
        calls: Arc<AtomicUsize>,
        credentials: Credentials,
    }

    impl ProvideCredentials for CountingCredentials {
        fn provide_credentials<'a>(&'a self) -> future::ProvideCredentials<'a>
        where
            Self: 'a,
        {
            self.calls.fetch_add(1, Ordering::Relaxed);
            future::ProvideCredentials::ready(provider::Result::Ok(self.credentials.clone()))
        }
    }
}
