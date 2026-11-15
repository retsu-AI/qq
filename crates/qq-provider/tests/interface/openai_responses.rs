use futures_util::StreamExt;
use qq_provider::{
    HttpAuth, HttpProtocol, Message, ModelRequest, ProviderErrorKind, test_support::LoopbackServer,
};

use crate::support::{
    assert_401_classifies_as_authentication, assert_output_limit_ends_the_stream,
    assert_output_truncation_is_typed, compile_http, oversized_text,
};

#[tokio::test]
async fn streamed_context_overflow_is_typed_for_runtime_recovery() {
    for event in [
        r#"{"type":"response.failed","response":{"error":{"code":"context_length_exceeded","message":"input exceeds the window"}}}"#,
        r#"{"type":"error","error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"input exceeds the window"}}"#,
        r#"{"type":"error","code":"context_length_exceeded","message":"input exceeds the window"}"#,
        r#"{"type":"response.failed","response":{"error":{"code":"context_length_exceeded"}}}"#,
    ] {
        let server = LoopbackServer::sse(format!("data: {event}\n\n"));
        let provider = compile_http(
            format!("{}/v1", server.base_url),
            HttpProtocol::OpenAiResponses,
            HttpAuth::NoAuth,
        );
        let events = provider
            .stream(ModelRequest::new(
                "test-model",
                vec![Message::user("hello")],
                64,
            ))
            .collect::<Vec<_>>()
            .await;
        server.capture();
        let [Err(error)] = &events[..] else {
            panic!("expected one terminal error for {event}: {events:?}");
        };
        assert_eq!(error.kind(), ProviderErrorKind::ContextExceeded, "{event}");
    }
}

#[tokio::test]
async fn classifies_a_401_rejection_as_an_authentication_failure() {
    assert_401_classifies_as_authentication(
        HttpProtocol::OpenAiResponses,
        "/v1",
        r#"{"error":{"message":"invalid key"}}"#,
    )
    .await;
}

#[tokio::test]
async fn enforces_the_output_byte_limit_end_to_end() {
    assert_output_limit_ends_the_stream(
        HttpProtocol::OpenAiResponses,
        format!(
            "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{}\"}}\n\n",
            oversized_text()
        ),
    )
    .await;
}

#[tokio::test]
async fn types_an_output_token_truncation_end_to_end() {
    assert_output_truncation_is_typed(
        HttpProtocol::OpenAiResponses,
        concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            "data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"},\"usage\":{\"input_tokens\":5,\"output_tokens\":64}}}\n\n",
        )
        .to_owned(),
        Some(64),
    )
    .await;
}
