use std::time::Duration;

use futures_util::StreamExt;
use qq_provider::{
    AttemptPolicy, EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, Message, ModelRequest,
    Provider, ProviderCompiler, ProviderError, ProviderEvent, ProviderRecipe, ReasoningEffort,
    RequestCredential, RequestCredentialFuture, RequestCredentialProvider,
    SharedRequestCredentialProvider,
    test_support::{CapturedRequest, LoopbackServer},
};

const RESPONSES_DONE: &str = "data: {\"type\":\"response.completed\"}\n\n";
const CHAT_DONE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: [DONE]\n\n",
);

fn efforts() -> [(ReasoningEffort, &'static str); 7] {
    [
        (ReasoningEffort::None, "none"),
        (ReasoningEffort::Minimal, "minimal"),
        (ReasoningEffort::Low, "low"),
        (ReasoningEffort::Medium, "medium"),
        (ReasoningEffort::High, "high"),
        (ReasoningEffort::Xhigh, "xhigh"),
        (ReasoningEffort::Max, "max"),
    ]
}

fn compile(
    server: &LoopbackServer,
    protocol: HttpProtocol,
    auth: HttpAuth,
    attempts: AttemptPolicy,
) -> std::sync::Arc<dyn Provider> {
    let endpoint = match &auth {
        HttpAuth::Codex { .. } | HttpAuth::RequestTimeCodex(_) => EndpointSpec::exact(
            format!("{}/backend-api/codex/responses", server.base_url),
            true,
        ),
        _ => EndpointSpec::base(format!("{}/v1", server.base_url), true),
    };
    ProviderCompiler::new()
        .unwrap()
        .with_attempt_policy(attempts)
        .compile(ProviderRecipe::http(HttpProviderRecipe::new(
            endpoint, protocol, auth,
        )))
        .unwrap()
}

async fn send(
    server: LoopbackServer,
    protocol: HttpProtocol,
    auth: HttpAuth,
    effort: Option<ReasoningEffort>,
    attempts: AttemptPolicy,
) -> Vec<CapturedRequest> {
    let provider = compile(&server, protocol, auth, attempts);
    let mut request = ModelRequest::new("test-model", vec![Message::user("hello")], 64);
    if let Some(effort) = effort {
        request = request.with_reasoning_effort(effort);
    }
    let events = provider.stream(request).collect::<Vec<_>>().await;
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Ok(ProviderEvent::Completed { .. }))),
        "provider did not complete: {events:?}"
    );
    server.capture_all()
}

fn static_codex_auth() -> HttpAuth {
    HttpAuth::Codex {
        access_token: "test-codex-token".into(),
        account_id: "test-account".into(),
        is_fedramp: false,
    }
}

struct StaticCodexRequestCredentials;

impl RequestCredentialProvider for StaticCodexRequestCredentials {
    fn credential(&self) -> RequestCredentialFuture<'_> {
        Box::pin(async { RequestCredential::codex("test-codex-token", "test-account", false) })
    }
}

fn request_time_codex_auth() -> HttpAuth {
    HttpAuth::RequestTimeCodex(SharedRequestCredentialProvider::new(
        StaticCodexRequestCredentials,
    ))
}

#[tokio::test]
async fn every_effort_reaches_responses_standard_codex_and_chat_transport() {
    for (effort, expected) in efforts() {
        for (protocol, auth, response, field) in [
            (
                HttpProtocol::OpenAiResponses,
                HttpAuth::NoAuth,
                RESPONSES_DONE,
                "responses",
            ),
            (
                HttpProtocol::OpenAiResponses,
                static_codex_auth(),
                RESPONSES_DONE,
                "codex",
            ),
            (
                HttpProtocol::OpenAiResponses,
                request_time_codex_auth(),
                RESPONSES_DONE,
                "request-time codex",
            ),
            (
                HttpProtocol::OpenAiChatCompletions,
                HttpAuth::NoAuth,
                CHAT_DONE,
                "chat",
            ),
        ] {
            let requests = send(
                LoopbackServer::sse(response),
                protocol,
                auth,
                Some(effort),
                AttemptPolicy::disabled(),
            )
            .await;
            let body = requests[0].json_body();
            let observed = if field == "chat" {
                body["reasoning_effort"].as_str()
            } else {
                body["reasoning"]["effort"].as_str()
            };
            assert_eq!(observed, Some(expected), "{field} lost {effort:?}");
        }
    }
}

#[tokio::test]
async fn omitted_effort_keeps_legacy_transport_bodies_byte_exact() {
    let cases = [
        (
            HttpProtocol::OpenAiResponses,
            HttpAuth::NoAuth,
            RESPONSES_DONE,
            r#"{"model":"test-model","input":[{"role":"user","content":"hello"}],"max_output_tokens":64,"stream":true,"store":false}"#,
        ),
        (
            HttpProtocol::OpenAiResponses,
            static_codex_auth(),
            RESPONSES_DONE,
            r#"{"model":"test-model","input":[{"role":"user","content":"hello"}],"stream":true,"store":false}"#,
        ),
        (
            HttpProtocol::OpenAiResponses,
            request_time_codex_auth(),
            RESPONSES_DONE,
            r#"{"model":"test-model","input":[{"role":"user","content":"hello"}],"stream":true,"store":false}"#,
        ),
        (
            HttpProtocol::OpenAiChatCompletions,
            HttpAuth::NoAuth,
            CHAT_DONE,
            r#"{"model":"test-model","messages":[{"role":"user","content":"hello"}],"stream":true,"stream_options":{"include_usage":true},"max_tokens":64}"#,
        ),
    ];
    for (protocol, auth, response, expected) in cases {
        let requests = send(
            LoopbackServer::sse(response),
            protocol,
            auth,
            None,
            AttemptPolicy::disabled(),
        )
        .await;
        assert_eq!(requests[0].body(), expected);
    }
}

#[tokio::test]
async fn actual_retry_preserves_effort_in_every_openai_request() {
    for (protocol, auth, response, chat) in [
        (
            HttpProtocol::OpenAiResponses,
            HttpAuth::NoAuth,
            RESPONSES_DONE,
            false,
        ),
        (
            HttpProtocol::OpenAiResponses,
            static_codex_auth(),
            RESPONSES_DONE,
            false,
        ),
        (
            HttpProtocol::OpenAiResponses,
            request_time_codex_auth(),
            RESPONSES_DONE,
            false,
        ),
        (
            HttpProtocol::OpenAiChatCompletions,
            HttpAuth::NoAuth,
            CHAT_DONE,
            true,
        ),
    ] {
        let server = LoopbackServer::respond_sequence(vec![
            (
                503,
                Some("application/json"),
                vec![b"{\"error\":\"retry\"}".to_vec()],
            ),
            (
                200,
                Some("text/event-stream"),
                vec![response.as_bytes().to_vec()],
            ),
        ]);
        let requests = send(
            server,
            protocol,
            auth,
            Some(ReasoningEffort::Xhigh),
            AttemptPolicy::new(2, Duration::ZERO, Duration::ZERO, Duration::from_secs(1)),
        )
        .await;
        assert_eq!(requests.len(), 2);
        for request in requests {
            let body = request.json_body();
            if chat {
                assert_eq!(body["reasoning_effort"], "xhigh");
            } else {
                assert_eq!(body["reasoning"]["effort"], "xhigh");
            }
        }
    }
}

#[tokio::test]
async fn anthropic_efforts_use_output_config_and_default_is_omitted() {
    for effort in [
        None,
        Some(ReasoningEffort::Low),
        Some(ReasoningEffort::Medium),
        Some(ReasoningEffort::High),
        Some(ReasoningEffort::Xhigh),
        Some(ReasoningEffort::Max),
    ] {
        let server = LoopbackServer::respond_sequence(vec![(200, Some("text/event-stream"), vec![b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec()])]);
        let requests = send(
            server,
            HttpProtocol::AnthropicMessages,
            HttpAuth::ApiKey("test".into()),
            effort,
            AttemptPolicy::new(1, Duration::ZERO, Duration::ZERO, Duration::from_secs(1)),
        )
        .await;
        let body = requests[0].json_body();
        match effort {
            Some(effort) => assert_eq!(body["output_config"]["effort"], effort.as_str()),
            None => assert!(body.get("output_config").is_none()),
        }
        assert!(body.get("thinking").is_none());
    }
}

struct PanicCredentials;

impl RequestCredentialProvider for PanicCredentials {
    fn credential(&self) -> RequestCredentialFuture<'_> {
        panic!("unsupported effort reached request-time authorization")
    }
}

#[tokio::test]
async fn unsupported_http_adapters_reject_before_auth_or_transport() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}/provider", listener.local_addr().unwrap());
    for (protocol, expected_adapter) in [
        (HttpProtocol::AnthropicMessages, "Anthropic Messages"),
        (
            HttpProtocol::GoogleGenerateContent,
            "Google GenerateContent",
        ),
    ] {
        let auth = if protocol == HttpProtocol::GoogleGenerateContent {
            HttpAuth::ApiKey("google-test-secret".into())
        } else {
            HttpAuth::RequestTimeBearer(SharedRequestCredentialProvider::new(PanicCredentials))
        };
        let provider = ProviderCompiler::new()
            .unwrap()
            .compile(ProviderRecipe::http(HttpProviderRecipe::new(
                EndpointSpec::exact(endpoint.clone(), true),
                protocol,
                auth,
            )))
            .unwrap();
        let events = provider
            .stream(
                ModelRequest::new("test-model", vec![Message::user("hello")], 64)
                    .with_reasoning_effort(ReasoningEffort::None),
            )
            .collect::<Vec<_>>()
            .await;
        assert!(matches!(
            events.as_slice(),
            [Err(ProviderError::Configuration(message))]
                if message.contains(expected_adapter)
        ));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "{expected_adapter} attempted transport before rejecting effort"
        );
    }
}
