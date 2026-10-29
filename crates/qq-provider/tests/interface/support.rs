use std::sync::Arc;

use futures_util::StreamExt;
use qq_provider::{
    EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, IncompleteReason, Message,
    ModelRequest, Provider, ProviderCompiler, ProviderError, ProviderErrorKind, ProviderEvent,
    ProviderRecipe, test_support::LoopbackServer,
};

pub fn compile_http(base_url: String, protocol: HttpProtocol, auth: HttpAuth) -> Arc<dyn Provider> {
    ProviderCompiler::new()
        .expect("compiler must construct")
        .compile(ProviderRecipe::http(HttpProviderRecipe::new(
            EndpointSpec::base(base_url, true),
            protocol,
            auth,
        )))
        .expect("loopback recipe must compile")
}

pub async fn assert_401_classifies_as_authentication(
    protocol: HttpProtocol,
    base_path: &str,
    error_body: &str,
) {
    let server = LoopbackServer::respond(401, "application/json", error_body);
    let provider = compile_http(
        format!("{}{base_path}", server.base_url),
        protocol,
        HttpAuth::ApiKey("interface-test-secret".into()),
    );

    let events = provider
        .stream(ModelRequest::new(
            "test-model",
            vec![Message::user("hello")],
            64,
        ))
        .collect::<Vec<_>>()
        .await;

    let [Err(error)] = &events[..] else {
        panic!("{protocol:?} must yield exactly one error event: {events:?}");
    };
    assert!(
        matches!(error, ProviderError::Api { status: 401, .. }),
        "{protocol:?} must preserve the 401 status: {error:?}"
    );
    assert_eq!(
        error.kind(),
        ProviderErrorKind::Authentication,
        "{protocol:?} must classify 401 as an authentication failure"
    );
    server.capture();
}

pub async fn assert_output_limit_ends_the_stream(protocol: HttpProtocol, sse_body: String) {
    let server = LoopbackServer::sse(sse_body);
    let provider = compile_http(
        server.base_url.clone(),
        protocol,
        HttpAuth::ApiKey("interface-test-secret".into()),
    );

    let events = provider
        .stream(ModelRequest::new(
            "test-model",
            vec![Message::user("hello")],
            1,
        ))
        .collect::<Vec<_>>()
        .await;

    let Some(Err(error)) = events.last() else {
        panic!("{protocol:?} must end the stream with an error: {events:?}");
    };
    assert!(
        matches!(error, ProviderError::Protocol(message)
            if message.contains("exceeded the configured size limit")),
        "{protocol:?} must report the exceeded limit: {error:?}"
    );
    server.capture();
}

pub fn oversized_text() -> String {
    "a".repeat(64 * 1_024 + 1)
}

pub async fn assert_output_truncation_is_typed(
    protocol: HttpProtocol,
    sse_body: String,
    expected_output_tokens: Option<u64>,
) {
    let server = LoopbackServer::sse(sse_body);
    let provider = compile_http(
        server.base_url.clone(),
        protocol,
        HttpAuth::ApiKey("interface-test-secret".into()),
    );

    let events = provider
        .stream(ModelRequest::new(
            "test-model",
            vec![Message::user("hello")],
            64,
        ))
        .collect::<Vec<_>>()
        .await;

    let text = events
        .iter()
        .filter_map(|event| match event {
            Ok(ProviderEvent::OutputTextDelta { text }) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(
        text, "partial",
        "{protocol:?} must keep the streamed prefix"
    );
    let Some(Ok(ProviderEvent::Incomplete { usage, reason })) = events.last() else {
        panic!("{protocol:?} must end a truncated stream with Incomplete: {events:?}");
    };
    assert_eq!(*reason, IncompleteReason::OutputTokens);
    assert_eq!(
        usage.map(|usage| usage.output_tokens),
        expected_output_tokens,
        "{protocol:?} must carry the reported usage on the terminal event"
    );
    server.capture();
}
