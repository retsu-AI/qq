use std::time::Duration;

use futures_util::StreamExt;
use qq_provider::{
    AttemptPolicy, EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, Message, ModelRequest,
    ProviderCompiler, ProviderError, ProviderErrorKind, ProviderEvent, ProviderRecipe, RetryOwner,
    test_support::LoopbackServer,
};

const CUT_OFF: [(HttpProtocol, &str); 4] = [
    (
        HttpProtocol::OpenAiResponses,
        concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Reading.\"}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,",
            "\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_1\",",
            "\"name\":\"read_file\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",",
            "\"delta\":\"{\\\"path\\\":\"}\n\n",
        ),
    ),
    (
        HttpProtocol::OpenAiChatCompletions,
        concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Reading.\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",",
            "\"type\":\"function\",\"function\":{\"name\":\"read_file\",",
            "\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
        ),
    ),
    (
        HttpProtocol::AnthropicMessages,
        concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Reading.\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"read_file\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
        ),
    ),
    (
        HttpProtocol::GoogleGenerateContent,
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Reading.\"}]},\"index\":0}]}\n\n",
    ),
];

fn compile(
    server: &LoopbackServer,
    protocol: HttpProtocol,
) -> std::sync::Arc<dyn qq_provider::Provider> {
    ProviderCompiler::new()
        .unwrap()
        .with_attempt_policy(AttemptPolicy::new(
            4,
            Duration::from_millis(1),
            Duration::from_millis(1),
            Duration::from_secs(5),
        ))
        .compile(ProviderRecipe::http(HttpProviderRecipe::new(
            EndpointSpec::base(format!("{}/v1", server.base_url), true),
            protocol,
            HttpAuth::ApiKey("interface-test-secret".into()),
        )))
        .unwrap()
}

fn request(owner: RetryOwner) -> ModelRequest {
    ModelRequest::new("test-model", vec![Message::user("hello")], 64).with_retry_owner(owner)
}

#[tokio::test]
async fn a_stream_cut_off_after_events_is_never_completed_and_never_resent() {
    for (protocol, body) in CUT_OFF {
        let server = LoopbackServer::sse(body);
        let provider = compile(&server, protocol);

        let events = provider
            .stream(request(RetryOwner::Provider))
            .collect::<Vec<_>>()
            .await;

        assert!(
            events
                .iter()
                .any(|event| matches!(event, Ok(ProviderEvent::OutputTextDelta { text }) if text == "Reading.")),
            "{protocol:?} must stream the prefix: {events:?}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                event,
                Ok(ProviderEvent::Completed { .. }
                    | ProviderEvent::Incomplete { .. }
                    | ProviderEvent::ToolCallCompleted { .. })
            )),
            "{protocol:?} must not complete a cut-off stream or its partial call: {events:?}"
        );
        let Some(Err(error)) = events.last() else {
            panic!("{protocol:?} must end with an error: {events:?}");
        };
        assert_eq!(
            error.kind(),
            ProviderErrorKind::Protocol,
            "{protocol:?}: {error}"
        );
        assert_eq!(
            server.capture_all().len(),
            1,
            "{protocol:?} must not resend"
        );
    }
}

#[tokio::test]
async fn an_empty_body_is_resent_only_while_the_provider_owns_retries() {
    for (protocol, done) in [
        (
            HttpProtocol::OpenAiResponses,
            "data: {\"type\":\"response.completed\"}\n\n",
        ),
        (
            HttpProtocol::OpenAiChatCompletions,
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        ),
        (
            HttpProtocol::AnthropicMessages,
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ),
        (
            HttpProtocol::GoogleGenerateContent,
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"ok\"}]},\"finishReason\":\"STOP\",\"index\":0}]}\n\n",
        ),
    ] {
        let empty = (200, Some("text/event-stream"), Vec::new());
        let server = LoopbackServer::respond_sequence(vec![
            empty.clone(),
            (
                200,
                Some("text/event-stream"),
                vec![done.as_bytes().to_vec()],
            ),
        ]);
        let events = compile(&server, protocol)
            .stream(request(RetryOwner::Provider))
            .collect::<Vec<_>>()
            .await;
        assert!(
            matches!(events.last(), Some(Ok(ProviderEvent::Completed { .. }))),
            "{protocol:?}: {events:?}"
        );
        assert_eq!(server.capture_all().len(), 2, "{protocol:?}");

        let server = LoopbackServer::respond_sequence(vec![empty]);
        let events = compile(&server, protocol)
            .stream(request(RetryOwner::Caller))
            .collect::<Vec<_>>()
            .await;
        let [Err(error)] = &events[..] else {
            panic!("{protocol:?} caller-owned empty body must fail once: {events:?}");
        };
        assert_eq!(error.kind(), ProviderErrorKind::Transport, "{protocol:?}");
        assert_eq!(server.capture_all().len(), 1, "{protocol:?}");
    }
}

#[tokio::test]
async fn a_caller_owned_request_hands_back_the_first_retryable_rejection() {
    for protocol in [
        HttpProtocol::OpenAiResponses,
        HttpProtocol::OpenAiChatCompletions,
        HttpProtocol::AnthropicMessages,
        HttpProtocol::GoogleGenerateContent,
    ] {
        let server = LoopbackServer::respond_with_headers(
            429,
            "application/json",
            &[("Retry-After", "7")],
            "{}",
        );
        let events = compile(&server, protocol)
            .stream(request(RetryOwner::Caller))
            .collect::<Vec<_>>()
            .await;
        let [Err(error)] = &events[..] else {
            panic!("{protocol:?}: {events:?}");
        };
        assert!(
            matches!(error, ProviderError::RetryAfter { status: 429, .. }),
            "{protocol:?}: {error:?}"
        );
        assert_eq!(error.retry_after(), Some(Duration::from_secs(7)));
        server.capture();
    }
}
