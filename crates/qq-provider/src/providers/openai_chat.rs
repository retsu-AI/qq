//! OpenAI-compatible Chat Completions API adapter.

use std::{borrow::Cow, sync::Arc};

use async_stream::try_stream;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};

use crate::{
    ContentBlock, IncompleteReason, Message, ModelRequest, Provider, ProviderError,
    ProviderErrorKind, ProviderEvent, ProviderStream, ProviderUsage, Role, ToolSpec,
    credentials::{SecretLiteral, sensitive_bearer_value, sensitive_header_value},
    exchange::{ContentTypeGate, SseExchangeSpec, sse_exchange, with_restart},
    http::{
        ExchangeMessages, HttpExchange, HttpRejection, SafeHeaders, is_request_controlled_header,
    },
    limits::{ByteCounter, StreamLimits},
    providers::support::{
        self, OwnedOrBorrowedText, ToolCallLedger, UsageOnce, status_error_kind,
        subtract_cached_input_tokens, value_as_status,
    },
    request_auth::RequestAuthorizer,
    sanitize::sanitize_message,
    sse::{SseDecoder, Utf8ErrorMessage},
};

#[cfg(test)]
use crate::http::validate_endpoint;

#[cfg(test)]
const CHAT_COMPLETIONS_ENDPOINT: &str = "https://api.openai.com/v1/chat/completions";

const SSE_SPEC: SseExchangeSpec = SseExchangeSpec {
    messages: ExchangeMessages {
        wire_overflow: "OpenAI-compatible wire size overflowed",
        wire_limit: "OpenAI-compatible stream exceeded the configured wire size limit",
    },
    non_sse_response: "OpenAI-compatible provider returned a non-SSE response",
    content_type_gate: ContentTypeGate::Strict,
};

/// Authentication applied by an OpenAI-compatible Chat Completions client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ChatCompletionsAuth {
    NoAuth,
    Bearer(SecretLiteral),
    Header(String, SecretLiteral),
}

/// A client for OpenAI-compatible Chat Completions endpoints.
pub(crate) struct OpenAiChatCompletions {
    pub(crate) exchange: HttpExchange,
    endpoint: reqwest::Url,
    headers: Arc<HeaderMap>,
}

#[cfg(test)]
impl OpenAiChatCompletions {
    /// One attempt per request, so a scripted single-response server is
    /// observed exactly once.
    fn single_shot(mut self) -> Self {
        self.exchange
            .set_attempt_policy(crate::http::AttemptPolicy::disabled());
        self
    }
}

impl OpenAiChatCompletions {
    /// Creates a client for OpenAI's standard Chat Completions endpoint.
    #[cfg(test)]
    pub(crate) fn new(api_key: &str) -> Result<Self, ProviderError> {
        Self::with_endpoint(
            CHAT_COMPLETIONS_ENDPOINT,
            ChatCompletionsAuth::Bearer(api_key.into()),
            [],
            false,
        )
    }

    /// Creates a client for an exact OpenAI-compatible endpoint URL.
    ///
    /// Plain HTTP is accepted only when `allow_http` is true and the URL host is
    /// loopback. Header names and values are validated while constructing the client.
    #[cfg(test)]
    pub(crate) fn with_endpoint(
        endpoint: &str,
        auth: ChatCompletionsAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        allow_http: bool,
    ) -> Result<Self, ProviderError> {
        let endpoint = validate_endpoint(endpoint, allow_http)?;
        let client = support::client_for_endpoint(&endpoint)?;
        Self::with_client_and_authorizer(
            client,
            endpoint,
            auth,
            static_headers,
            RequestAuthorizer::default(),
        )
        .map(Self::single_shot)
    }

    pub(crate) fn with_client_and_authorizer(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        auth: ChatCompletionsAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        authorizer: RequestAuthorizer,
    ) -> Result<Self, ProviderError> {
        let (headers, redactions) = build_headers(auth, static_headers)?;

        Ok(Self {
            exchange: HttpExchange::new(client, authorizer, Arc::from(redactions)),
            endpoint,
            headers: Arc::new(headers),
        })
    }
}

impl Provider for OpenAiChatCompletions {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let exchange = self.exchange.clone();
        let endpoint = self.endpoint.clone();
        let headers = self.headers.clone();

        with_restart(&self.exchange, move |ledger| {
            let exchange = exchange.clone();
            let endpoint = endpoint.clone();
            let headers = headers.clone();
            let request = request.clone();
            Box::pin(try_stream! {
                let limits = StreamLimits::new(request.max_output_tokens());
                let body = ChatCompletionsRequest::from(&request);
                let mut sse = sse_exchange(
                    &exchange,
                    (endpoint, HeaderMap::clone(&headers)),
                    (&body, request.wire_size_hint()),
                    sse_decoder(limits.event),
                    limits.wire,
                    SSE_SPEC,
                    &ledger,
                )
                .await
                .map_err(|error| error.into_provider_error(api_error))?;

                let redactions = Arc::clone(sse.redactions());
                let mut output_bytes = ByteCounter::new(
                    limits.output,
                    "OpenAI-compatible output size overflowed",
                    "OpenAI-compatible output exceeded the configured size limit",
                );
                let mut usage = UsageOnce::new(
                    "OpenAI-compatible stream reported usage more than once",
                );
                // Maps streamed tool-call array indexes to call ids so argument
                // fragments and the finish reason can be attributed after the
                // first fragment.
                let mut tool_calls = ToolCallLedger::new(
                    "OpenAI-compatible stream reused a tool-call index",
                    "OpenAI-compatible stream sent arguments for an unknown tool call",
                );

                // A `length` finish reason arrives before the usage chunk and
                // `[DONE]`; remember it so the terminal event still carries usage.
                let mut incomplete = None;

                while let Some(event) = sse.next_event().await? {
                    let data = event.data.trim();
                    if data.is_empty() {
                        continue;
                    }
                    if data == "[DONE]" {
                        let usage = usage.finish();
                        match incomplete {
                            Some(reason) => yield ProviderEvent::Incomplete { usage, reason },
                            None => yield ProviderEvent::Completed { usage },
                        }
                        return;
                    }

                    let decoded = decode_event(data, redactions.as_ref())?;
                    if let Some(chunk_usage) = decoded.usage {
                        usage.set(chunk_usage)?;
                    }
                    for delta in decoded.deltas {
                        match delta {
                            DecodedDelta::OutputText(text) => {
                                output_bytes.add(text.len())?;
                                yield ProviderEvent::OutputTextDelta { text };
                            }
                            DecodedDelta::Refusal(text) => {
                                output_bytes.add(text.len())?;
                                yield ProviderEvent::RefusalDelta { text };
                            }
                            DecodedDelta::ToolCallStarted { index, id, name } => {
                                tool_calls.insert(index, id.clone())?;
                                yield ProviderEvent::ToolCallStarted { id, name };
                            }
                            DecodedDelta::ToolCallArguments { index, json } => {
                                let id = tool_calls.get(&index)?.to_owned();
                                output_bytes.add(json.len())?;
                                yield ProviderEvent::ToolCallArgumentsDelta { id, json };
                            }
                            DecodedDelta::ToolCallsFinished => {
                                for id in tool_calls.drain() {
                                    yield ProviderEvent::ToolCallCompleted { id };
                                }
                            }
                            DecodedDelta::Incomplete(reason) => {
                                incomplete = Some(reason);
                            }
                            DecodedDelta::TerminalError(error) => Err(error)?,
                        }
                    }
                }

                Err(sse.ended_early("OpenAI-compatible stream ended before [DONE]"))?;
            })
        })
    }
}

fn build_headers(
    auth: ChatCompletionsAuth,
    static_headers: impl IntoIterator<Item = (String, String)>,
) -> Result<(HeaderMap, Vec<String>), ProviderError> {
    let mut redactions = Vec::new();
    let auth_header = match auth {
        ChatCompletionsAuth::NoAuth => None,
        ChatCompletionsAuth::Bearer(secret) => {
            let value = sensitive_bearer_value(&secret, "Bearer secret")?;
            redactions.push(secret.expose_secret().to_owned());
            Some((AUTHORIZATION, value))
        }
        ChatCompletionsAuth::Header(name, secret) => {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ProviderError::Configuration("authentication header name is invalid".to_owned())
            })?;
            if is_request_controlled_header(&name) {
                return Err(ProviderError::Configuration(
                    "authentication header is controlled by the HTTP client".to_owned(),
                ));
            }
            let value = sensitive_header_value(&secret, "authentication header secret")?;
            redactions.push(secret.expose_secret().to_owned());
            Some((name, value))
        }
    };
    let auth_name = auth_header.as_ref().map(|(name, _)| name.clone());
    let mut headers = SafeHeaders::new(std::iter::once(AUTHORIZATION).chain(auth_name));
    headers.insert_configured(static_headers, false)?;
    if let Some((name, value)) = auth_header {
        headers.insert_owned(name, value);
    }
    for redaction in redactions {
        headers.push_redaction(redaction);
    }
    Ok(headers.finish())
}

fn sse_decoder(max_event_bytes: usize) -> SseDecoder {
    SseDecoder::data_only(
        max_event_bytes,
        "OpenAI-compatible SSE event size overflowed",
        "OpenAI-compatible SSE event exceeded the configured size limit",
        Utf8ErrorMessage::WithSource("OpenAI-compatible SSE event was not UTF-8"),
    )
}

#[derive(Serialize)]
pub(crate) struct ChatCompletionsRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ChatTool<'a>>,
    stream: bool,
    stream_options: ChatStreamOptions,
    max_tokens: u32,
}

impl<'a> From<&'a ModelRequest> for ChatCompletionsRequest<'a> {
    fn from(request: &'a ModelRequest) -> Self {
        let mut messages = Vec::with_capacity(request.messages().len() + 1);
        // Chat Completions has no dedicated instructions slot; the system
        // prompt travels as the leading system-role message.
        if let Some(system) = request.system() {
            messages.push(ChatMessage {
                role: ChatRole::System,
                content: Some(OwnedOrBorrowedText(Cow::Borrowed(system))),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        for message in request.messages() {
            append_chat_messages(message, &mut messages);
        }
        Self {
            model: request.model(),
            messages,
            tools: request.tools().iter().map(ChatTool::from).collect(),
            stream: true,
            stream_options: ChatStreamOptions {
                include_usage: true,
            },
            max_tokens: request.max_output_tokens(),
        }
    }
}

/// Appends the wire messages for one provider-neutral message.
///
/// Chat Completions has no multi-result message, so each `ToolResult` block
/// becomes its own `tool`-role message; `is_error` has no wire field because
/// the runtime frames errors inside the result content. Any remaining text and
/// tool calls follow as one message so a lone text block keeps its legacy
/// plain-string shape.
fn append_chat_messages<'a>(message: &'a Message, messages: &mut Vec<ChatMessage<'a>>) {
    let role = match message.role() {
        Role::User => ChatRole::User,
        Role::Assistant => ChatRole::Assistant,
    };
    if let [ContentBlock::Text { text }] = message.content() {
        messages.push(ChatMessage {
            role,
            content: Some(OwnedOrBorrowedText(Cow::Borrowed(text.as_str()))),
            tool_calls: None,
            tool_call_id: None,
        });
        return;
    }

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut wrote_results = false;
    for block in message.content() {
        match block {
            ContentBlock::Text { text: fragment } => text.push_str(fragment),
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => tool_calls.push(ChatToolCall::Function {
                id,
                function: ChatFunctionCall {
                    name,
                    arguments: arguments.get(),
                },
            }),
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error: _,
            } => {
                wrote_results = true;
                messages.push(ChatMessage {
                    role: ChatRole::Tool,
                    content: Some(OwnedOrBorrowedText(Cow::Borrowed(content.as_str()))),
                    tool_calls: None,
                    tool_call_id: Some(call_id),
                });
            }
        }
    }

    if !text.is_empty() || !tool_calls.is_empty() || !wrote_results {
        let content = if text.is_empty() && !tool_calls.is_empty() {
            None
        } else {
            Some(OwnedOrBorrowedText(Cow::Owned(text)))
        };
        messages.push(ChatMessage {
            role,
            content,
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            tool_call_id: None,
        });
    }
}

#[derive(Serialize)]
struct ChatStreamOptions {
    include_usage: bool,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: ChatRole,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<OwnedOrBorrowedText<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<ChatToolCall<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatToolCall<'a> {
    Function {
        id: &'a str,
        function: ChatFunctionCall<'a>,
    },
}

#[derive(Serialize)]
struct ChatFunctionCall<'a> {
    name: &'a str,
    /// Chat Completions carries tool arguments as a JSON-encoded string.
    arguments: &'a str,
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ChatTool<'a> {
    Function { function: ChatFunction<'a> },
}

impl<'a> From<&'a ToolSpec> for ChatTool<'a> {
    fn from(tool: &'a ToolSpec) -> Self {
        Self::Function {
            function: ChatFunction {
                name: tool.name(),
                description: tool.description(),
                parameters: tool.input_schema(),
            },
        }
    }
}

#[derive(Serialize)]
struct ChatFunction<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a RawValue,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum ChatRole {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Deserialize)]
struct ChatCompletionChunk {
    #[serde(default)]
    choices: Vec<ChatChoice>,
    usage: Option<ChatUsage>,
    error: Option<WireApiError>,
}

#[derive(Deserialize)]
struct ChatUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
    prompt_tokens_details: Option<ChatPromptTokenDetails>,
    completion_tokens_details: Option<ChatCompletionTokenDetails>,
}

#[derive(Deserialize)]
struct ChatPromptTokenDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Deserialize)]
struct ChatCompletionTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct ChatChoice {
    #[serde(default)]
    delta: ChatDelta,
    finish_reason: Option<String>,
}

#[derive(Default, Deserialize)]
struct ChatDelta {
    content: Option<String>,
    refusal: Option<String>,
    #[serde(default)]
    tool_calls: Vec<ChatToolCallDelta>,
}

#[derive(Deserialize)]
struct ChatToolCallDelta {
    index: u64,
    id: Option<String>,
    #[serde(default)]
    function: ChatFunctionDelta,
}

#[derive(Default, Deserialize)]
struct ChatFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error: WireApiError,
}

#[derive(Deserialize)]
struct WireApiError {
    message: Option<String>,
    code: Option<Value>,
    #[serde(rename = "type")]
    error_type: Option<String>,
    status: Option<Value>,
}

#[derive(Debug)]
enum DecodedDelta {
    OutputText(String),
    Refusal(String),
    ToolCallStarted {
        index: u64,
        id: String,
        name: String,
    },
    ToolCallArguments {
        index: u64,
        json: String,
    },
    ToolCallsFinished,
    /// The choice stopped short for a recoverable reason; the stream still
    /// runs to `[DONE]` so trailing usage is captured.
    Incomplete(IncompleteReason),
    TerminalError(ProviderError),
}

#[derive(Debug)]
struct DecodedChunk {
    deltas: Vec<DecodedDelta>,
    usage: Option<ProviderUsage>,
}

fn decode_event(data: &str, redactions: &[String]) -> Result<DecodedChunk, ProviderError> {
    let chunk: ChatCompletionChunk = serde_json::from_str(data).map_err(|error| {
        ProviderError::Protocol(sanitize_message(
            &format!("could not decode OpenAI-compatible event: {error}"),
            redactions,
        ))
    })?;

    if let Some(error) = chunk.error {
        return Err(wire_api_error(error, redactions));
    }

    let usage = chunk.usage.map(provider_usage).transpose()?;
    let mut deltas = Vec::new();
    for choice in chunk.choices {
        if let Some(text) = choice.delta.content.filter(|text| !text.is_empty()) {
            deltas.push(DecodedDelta::OutputText(text));
        }
        if let Some(text) = choice.delta.refusal.filter(|text| !text.is_empty()) {
            deltas.push(DecodedDelta::Refusal(text));
        }
        for tool_call in choice.delta.tool_calls {
            // The first fragment of a call carries `id` and `function.name`;
            // later fragments carry only `function.arguments`.
            if let Some(id) = tool_call.id {
                let Some(name) = tool_call.function.name else {
                    return Err(ProviderError::Protocol(
                        "OpenAI-compatible stream started a tool call without a name".to_owned(),
                    ));
                };
                deltas.push(DecodedDelta::ToolCallStarted {
                    index: tool_call.index,
                    id,
                    name,
                });
            }
            if let Some(json) = tool_call.function.arguments.filter(|json| !json.is_empty()) {
                deltas.push(DecodedDelta::ToolCallArguments {
                    index: tool_call.index,
                    json,
                });
            }
        }
        if let Some(reason) = choice.finish_reason {
            let error = match reason.as_str() {
                "stop" => continue,
                "tool_calls" => {
                    deltas.push(DecodedDelta::ToolCallsFinished);
                    continue;
                }
                "length" => {
                    deltas.push(DecodedDelta::Incomplete(IncompleteReason::OutputTokens));
                    continue;
                }
                "content_filter" => ProviderError::ResponseIncomplete(
                    "OpenAI-compatible response was stopped by a content filter".to_owned(),
                ),
                "function_call" => ProviderError::Protocol(
                    "OpenAI-compatible response requested legacy function execution".to_owned(),
                ),
                _ => ProviderError::Protocol(
                    "OpenAI-compatible response used an unsupported finish reason".to_owned(),
                ),
            };
            deltas.push(DecodedDelta::TerminalError(error));
        }
    }
    Ok(DecodedChunk { deltas, usage })
}

fn provider_usage(usage: ChatUsage) -> Result<ProviderUsage, ProviderError> {
    let cached = usage
        .prompt_tokens_details
        .map_or(0, |details| details.cached_tokens);
    let input_tokens = subtract_cached_input_tokens(
        usage.prompt_tokens,
        cached,
        "OpenAI-compatible cached input tokens exceeded prompt tokens",
    )?;
    Ok(ProviderUsage {
        input_tokens,
        cache_read_input_tokens: cached,
        cache_write_input_tokens: 0,
        output_tokens: usage.completion_tokens,
        reasoning_tokens: usage
            .completion_tokens_details
            .and_then(|details| details.reasoning_tokens),
    })
}

fn wire_api_error(error: WireApiError, redactions: &[String]) -> ProviderError {
    let kind = wire_error_kind(&error);
    let message = error.message.as_deref().map_or_else(
        || "OpenAI-compatible provider did not provide an error message".to_owned(),
        |message| sanitize_message(message, redactions),
    );
    ProviderError::ResponseFailed { kind, message }
}

fn wire_error_kind(error: &WireApiError) -> ProviderErrorKind {
    if let Some(status) = error.status.as_ref().and_then(value_as_status) {
        return status_error_kind(status);
    }
    if let Some(status) = error.code.as_ref().and_then(value_as_status) {
        return status_error_kind(status);
    }

    for name in [
        error.code.as_ref().and_then(Value::as_str),
        error.error_type.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let kind = named_error_kind(name);
        if kind != ProviderErrorKind::Response {
            return kind;
        }
    }

    ProviderErrorKind::Response
}

fn named_error_kind(name: &str) -> ProviderErrorKind {
    match name.to_ascii_lowercase().as_str() {
        "invalid_api_key" | "authentication_error" | "permission_error" => {
            ProviderErrorKind::Authentication
        }
        "rate_limit_exceeded" | "insufficient_quota" => ProviderErrorKind::RateLimited,
        "context_length_exceeded" => ProviderErrorKind::ContextExceeded,
        "invalid_prompt"
        | "invalid_request_error"
        | "invalid_value"
        | "model_not_found"
        | "unsupported_value" => ProviderErrorKind::InvalidRequest,
        "server_error" | "service_unavailable" => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Response,
    }
}

fn api_error(rejection: HttpRejection) -> ProviderError {
    support::api_error(
        rejection,
        "OpenAI-compatible request failed",
        |envelope: ApiErrorEnvelope| envelope.error.message,
    )
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use serde_json::json;

    use crate::{http::build_direct_client, test_support::LoopbackServer};

    use super::*;

    #[test]
    fn default_constructor_uses_openai_endpoint_and_redacts_auth_debug() {
        let provider = OpenAiChatCompletions::new("openai-test-secret").unwrap();
        let auth = ChatCompletionsAuth::Bearer("openai-test-secret".into());

        assert_eq!(provider.endpoint.as_str(), CHAT_COMPLETIONS_ENDPOINT);
        assert!(!format!("{auth:?}").contains("openai-test-secret"));
    }

    #[test]
    fn validates_http_endpoint_policy_and_url_components() {
        for (endpoint, allow_http) in [
            ("http://example.com/v1/chat/completions", true),
            ("http://127.0.0.1/v1/chat/completions", false),
            (
                "https://user:password@example.com/v1/chat/completions",
                false,
            ),
            ("https://example.com/v1/chat/completions#fragment", false),
        ] {
            let error = OpenAiChatCompletions::with_endpoint(
                endpoint,
                ChatCompletionsAuth::NoAuth,
                [],
                allow_http,
            )
            .err()
            .expect("endpoint must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }

        OpenAiChatCompletions::with_endpoint(
            "http://[::1]/v1/chat/completions",
            ChatCompletionsAuth::NoAuth,
            [],
            true,
        )
        .expect("IPv6 loopback HTTP should be accepted");
    }

    #[test]
    fn rejects_static_overrides_and_invalid_headers() {
        for name in [
            "authorization",
            "accept",
            "connection",
            "content-length",
            "content-type",
            "expect",
            "host",
            "http2-settings",
            "keep-alive",
            "proxy-authenticate",
            "proxy-authorization",
            "proxy-connection",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "user-agent",
        ] {
            let error = OpenAiChatCompletions::with_endpoint(
                "https://example.com/v1/chat/completions",
                ChatCompletionsAuth::NoAuth,
                [(name.to_owned(), "value".to_owned())],
                false,
            )
            .err()
            .expect("controlled header must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }

        let error = OpenAiChatCompletions::with_endpoint(
            "https://example.com/v1/chat/completions",
            ChatCompletionsAuth::Header("x-api-key".to_owned(), "secret".into()),
            [("x-api-key".to_owned(), "override".to_owned())],
            false,
        )
        .err()
        .expect("authentication header override must be rejected");
        assert!(matches!(error, ProviderError::Configuration(_)));

        for headers in [
            vec![
                ("x-test".to_owned(), "one".to_owned()),
                ("X-Test".to_owned(), "two".to_owned()),
            ],
            vec![("bad header".to_owned(), "value".to_owned())],
            vec![("x-test".to_owned(), "bad\r\nvalue".to_owned())],
        ] {
            let error = OpenAiChatCompletions::with_endpoint(
                "https://example.com/v1/chat/completions",
                ChatCompletionsAuth::NoAuth,
                headers,
                false,
            )
            .err()
            .expect("invalid header must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }
    }

    #[test]
    fn configured_headers_are_sensitive_and_redactions_are_normalized() {
        let (headers, redactions) = build_headers(
            ChatCompletionsAuth::Header("x-auth".to_owned(), "long-test-secret".into()),
            [
                ("x-first".to_owned(), "test-secret".to_owned()),
                ("x-second".to_owned(), "long-test-secret".to_owned()),
                ("x-third".to_owned(), "alpha".to_owned()),
            ],
        )
        .unwrap();

        assert!(
            headers
                .values()
                .all(reqwest::header::HeaderValue::is_sensitive)
        );
        assert_eq!(redactions, ["long-test-secret", "test-secret", "alpha"]);
        assert_eq!(
            sanitize_message("long-test-secret test-secret alpha", &redactions),
            "[REDACTED] [REDACTED] [REDACTED]"
        );
    }

    #[test]
    fn adapter_byte_accounting_preserves_error_text() {
        let mut output = ByteCounter::new(
            4,
            "OpenAI-compatible output size overflowed",
            "OpenAI-compatible output exceeded the configured size limit",
        );
        assert_eq!(
            output.add(5).unwrap_err().to_string(),
            "provider stream was invalid: OpenAI-compatible output exceeded the configured size limit"
        );

        let mut wire = ByteCounter::new(
            usize::MAX,
            "OpenAI-compatible wire size overflowed",
            "OpenAI-compatible stream exceeded the configured wire size limit",
        );
        wire.add(usize::MAX).unwrap();
        assert_eq!(
            wire.add(1).unwrap_err().to_string(),
            "provider stream was invalid: OpenAI-compatible wire size overflowed"
        );
    }

    #[test]
    fn decodes_fragmented_bom_comments_line_endings_and_multiline_data() {
        let source = concat!(
            "\u{feff}: comment\r\n",
            "data: {\"choices\":[{\"delta\":\r",
            "data: {\"content\":\"hello\"}}]}\n\r",
            "data: [DONE]\r\r",
        );
        let mut decoder = sse_decoder(1_024);
        let mut events = Vec::new();

        for byte in source.as_bytes() {
            events.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
        }

        assert_eq!(
            events
                .into_iter()
                .map(|event| event.data)
                .collect::<Vec<_>>(),
            [
                "{\"choices\":[{\"delta\":\n{\"content\":\"hello\"}}]}",
                "[DONE]",
            ]
        );
    }

    #[test]
    fn classifies_top_level_stream_errors() {
        let rate_limit_error = decode_event(
            r#"{"error":{"message":"slow down","code":"rate_limit_exceeded"}}"#,
            &[],
        )
        .unwrap_err();
        let authentication_error = decode_event(
            r#"{"error":{"message":"bad key","code":"provider_specific","type":"authentication_error"}}"#,
            &[],
        )
        .unwrap_err();

        assert!(matches!(
            rate_limit_error,
            ProviderError::ResponseFailed { .. }
        ));
        assert_eq!(rate_limit_error.kind(), ProviderErrorKind::RateLimited);
        assert_eq!(
            authentication_error.kind(),
            ProviderErrorKind::Authentication
        );
    }

    #[test]
    fn rejects_incomplete_and_unsupported_finish_reasons() {
        let length = decode_event(
            r#"{"choices":[{"delta":{},"finish_reason":"length"}]}"#,
            &[],
        )
        .unwrap()
        .deltas
        .pop()
        .expect("length must produce a terminal outcome");
        let function_call = decode_event(
            r#"{"choices":[{"delta":{},"finish_reason":"function_call"}]}"#,
            &[],
        )
        .unwrap()
        .deltas
        .pop()
        .expect("function call must produce a terminal outcome");
        let tool_calls = decode_event(
            r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
            &[],
        )
        .unwrap()
        .deltas
        .pop()
        .expect("tool calls must finish the open calls");

        assert!(matches!(
            length,
            DecodedDelta::Incomplete(IncompleteReason::OutputTokens)
        ));
        assert!(matches!(
            function_call,
            DecodedDelta::TerminalError(ProviderError::Protocol(_))
        ));
        assert!(matches!(tool_calls, DecodedDelta::ToolCallsFinished));
    }

    #[test]
    fn decodes_usage_and_rejects_cached_prompt_underflow() {
        let usage = decode_event(
            r#"{"choices":[],"usage":{"prompt_tokens":20,"completion_tokens":7,"prompt_tokens_details":{"cached_tokens":6},"completion_tokens_details":{"reasoning_tokens":2}}}"#,
            &[],
        )
        .unwrap()
        .usage;
        assert_eq!(
            usage,
            Some(ProviderUsage {
                input_tokens: 14,
                cache_read_input_tokens: 6,
                cache_write_input_tokens: 0,
                output_tokens: 7,
                reasoning_tokens: Some(2),
            })
        );

        let error = decode_event(
            r#"{"choices":[],"usage":{"prompt_tokens":2,"completion_tokens":1,"prompt_tokens_details":{"cached_tokens":3}}}"#,
            &[],
        )
        .unwrap_err();
        assert!(matches!(error, ProviderError::Protocol(_)));
    }

    #[test]
    fn enforces_event_output_and_wire_limits() {
        let event_error = sse_decoder(8)
            .push(b"data: this event keeps going")
            .unwrap_err();
        let mut output = ByteCounter::new(4, "output overflow", "output limit");
        let output_error = output.add(5).unwrap_err();
        let mut wire = ByteCounter::new(8, "wire overflow", "wire limit");
        let wire_error = wire.add(9).unwrap_err();

        assert!(matches!(event_error, ProviderError::Protocol(_)));
        assert!(matches!(output_error, ProviderError::Protocol(_)));
        assert!(matches!(wire_error, ProviderError::Protocol(_)));
    }

    #[tokio::test]
    async fn sends_exact_request_and_streams_fragmented_deltas() {
        let chunks = vec![
            b"\xef".to_vec(),
            b"\xbb".to_vec(),
            b"\xbf: heartbeat\r".to_vec(),
            b"\ndata: {\"choices\":[{\"delta\":\r\n".to_vec(),
            b"data: {\"content\":\"h\xc3".to_vec(),
            b"\xa9l\"}}]}\r\n\r".to_vec(),
            b"\ndata: {\"choices\":[{\"delta\":{\"refusal\":\"cannot\"}}]}\n\n".to_vec(),
            b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\r\r".to_vec(),
            b"data: {\"choices\":[],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":6}}}\n\n".to_vec(),
            b"data: [DO".to_vec(),
            b"NE]\r\n\r\n".to_vec(),
        ];
        let server =
            LoopbackServer::respond_chunks(200, Some("text/event-stream; charset=utf-8"), chunks);
        let endpoint = format!("{}/custom/chat/completions?api-version=42", server.base_url);
        let provider = OpenAiChatCompletions::with_endpoint(
            &endpoint,
            ChatCompletionsAuth::Header("x-api-key".to_owned(), "custom-test-secret".into()),
            [("x-client".to_owned(), "qq-tests".to_owned())],
            true,
        )
        .unwrap();
        let events = provider
            .stream(ModelRequest::new(
                "chat-test",
                vec![Message::user("ping"), Message::assistant("pong")],
                321,
            ))
            .collect::<Vec<_>>()
            .await;

        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::OutputTextDelta { text }) if text == "hél"
        ));
        assert!(matches!(
            &events[1],
            Ok(ProviderEvent::RefusalDelta { text }) if text == "cannot"
        ));
        assert_eq!(
            events[2].as_ref().unwrap(),
            &ProviderEvent::Completed {
                usage: Some(ProviderUsage {
                    input_tokens: 14,
                    cache_read_input_tokens: 6,
                    cache_write_input_tokens: 0,
                    output_tokens: 7,
                    reasoning_tokens: None,
                }),
            }
        );

        let request = server.capture();
        assert_eq!(
            request.request_line(),
            Some("POST /custom/chat/completions?api-version=42 HTTP/1.1")
        );
        assert_eq!(request.header("accept"), Some("text/event-stream"));
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert_eq!(request.header("x-api-key"), Some("custom-test-secret"));
        assert_eq!(request.header("x-client"), Some("qq-tests"));
        assert_eq!(request.header("authorization"), None);
        assert_eq!(
            request.json_body(),
            json!({
                "model": "chat-test",
                "messages": [
                    {"role": "user", "content": "ping"},
                    {"role": "assistant", "content": "pong"}
                ],
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_tokens": 321
            })
        );
    }

    #[test]
    fn maps_the_system_prompt_to_a_leading_system_message() {
        let request = ModelRequest::new("chat-test", vec![Message::user("ping")], 64)
            .with_system("You are QQ.");
        let body = serde_json::to_value(ChatCompletionsRequest::from(&request)).unwrap();
        assert_eq!(
            body["messages"],
            json!([
                {"role": "system", "content": "You are QQ."},
                {"role": "user", "content": "ping"}
            ])
        );

        let without = ModelRequest::new("chat-test", vec![Message::user("ping")], 64);
        let body = serde_json::to_value(ChatCompletionsRequest::from(&without)).unwrap();
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": "ping"}])
        );
    }

    #[tokio::test]
    async fn sends_tool_declarations_and_tool_history_messages() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = LoopbackServer::sse(body);
        let endpoint = format!("{}/v1/chat/completions", server.base_url);
        let provider =
            OpenAiChatCompletions::with_endpoint(&endpoint, ChatCompletionsAuth::NoAuth, [], true)
                .unwrap();
        let request = ModelRequest::new(
            "chat-test",
            vec![
                Message::user("read the config"),
                Message::new(
                    Role::Assistant,
                    vec![
                        ContentBlock::Text {
                            text: "Reading it now.".to_owned(),
                        },
                        ContentBlock::tool_call(
                            "call_1".to_owned(),
                            "read_file".to_owned(),
                            &json!({"path": "config.ron"}),
                        ),
                    ],
                ),
                Message::tool_results(vec![
                    ContentBlock::ToolResult {
                        call_id: "call_1".to_owned(),
                        content: "(config)".to_owned(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        call_id: "call_2".to_owned(),
                        content: "not found".to_owned(),
                        is_error: true,
                    },
                ]),
            ],
            128,
        )
        .with_tools(vec![ToolSpec::new(
            "read_file",
            "Reads one file",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )]);
        let events = provider.stream(request).collect::<Vec<_>>().await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::Completed { usage: None })
        ));

        assert_eq!(
            server.capture().json_body(),
            json!({
                "model": "chat-test",
                "messages": [
                    {"role": "user", "content": "read the config"},
                    {"role": "assistant", "content": "Reading it now.",
                     "tool_calls": [
                        {"id": "call_1", "type": "function",
                         "function": {"name": "read_file",
                                      "arguments": "{\"path\":\"config.ron\"}"}}
                     ]},
                    {"role": "tool", "tool_call_id": "call_1", "content": "(config)"},
                    {"role": "tool", "tool_call_id": "call_2", "content": "not found"}
                ],
                "tools": [
                    {"type": "function",
                     "function": {"name": "read_file", "description": "Reads one file",
                                  "parameters": {"type": "object",
                                                 "properties": {"path": {"type": "string"}}}}}
                ],
                "stream": true,
                "stream_options": {"include_usage": true},
                "max_tokens": 128
            })
        );
    }

    #[tokio::test]
    async fn streams_tool_calls_with_attributed_arguments_to_completion() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Checking.\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"\\\"a.rs\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        );
        let server = LoopbackServer::sse(body);
        let endpoint = format!("{}/v1/chat/completions", server.base_url);
        let provider =
            OpenAiChatCompletions::with_endpoint(&endpoint, ChatCompletionsAuth::NoAuth, [], true)
                .unwrap();
        let events = provider
            .stream(test_request())
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();

        assert_eq!(
            events,
            vec![
                ProviderEvent::OutputTextDelta {
                    text: "Checking.".to_owned(),
                },
                ProviderEvent::ToolCallStarted {
                    id: "call_1".to_owned(),
                    name: "read_file".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_1".to_owned(),
                    json: "{\"path\":".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_1".to_owned(),
                    json: "\"a.rs\"}".to_owned(),
                },
                ProviderEvent::ToolCallCompleted {
                    id: "call_1".to_owned(),
                },
                ProviderEvent::Completed { usage: None },
            ]
        );
        server.capture();
    }

    #[tokio::test]
    async fn rejects_tool_arguments_for_an_unknown_call() {
        let body = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":4,\"function\":{\"arguments\":\"{}\"}}]}}]}\n\n";
        let server = LoopbackServer::sse(body);
        let endpoint = format!("{}/v1/chat/completions", server.base_url);
        let provider =
            OpenAiChatCompletions::with_endpoint(&endpoint, ChatCompletionsAuth::NoAuth, [], true)
                .unwrap();
        let error = provider
            .stream(test_request())
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(error, ProviderError::Protocol(_)));
        server.capture();
    }

    #[tokio::test]
    async fn returns_typed_401_without_exposing_secrets() {
        let body = r#"{"error":{"message":"invalid test-api-secret\ncredential","type":"authentication_error"}}"#;
        let server = LoopbackServer::respond(401, "application/json", body);
        let endpoint = format!("{}/v1/chat/completions", server.base_url);
        let provider = OpenAiChatCompletions::with_endpoint(
            &endpoint,
            ChatCompletionsAuth::Bearer("test-api-secret".into()),
            [],
            true,
        )
        .unwrap();
        let error = provider
            .stream(test_request())
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert_eq!(error.kind(), ProviderErrorKind::Authentication);
        assert!(matches!(error, ProviderError::Api { status: 401, .. }));
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("test-api-secret"));
        assert!(!rendered.contains('\n'));

        let request = server.capture();
        assert_eq!(
            request.header("authorization"),
            Some("Bearer test-api-secret")
        );
    }

    #[tokio::test]
    async fn request_time_bearer_auth_is_applied_and_redacted_from_rejections() {
        const SECRET: &str = "dynamic-chat-test-secret";

        let body = format!(r#"{{"error":{{"message":"invalid {SECRET}"}}}}"#);
        let server = LoopbackServer::respond(401, "application/json", body);
        let endpoint = format!("{}/v1/chat/completions", server.base_url);
        let credentials =
            crate::SharedRequestCredentialProvider::new(StaticBearerCredential(SECRET));
        let provider = OpenAiChatCompletions::with_client_and_authorizer(
            build_direct_client().unwrap(),
            reqwest::Url::parse(&endpoint).unwrap(),
            ChatCompletionsAuth::NoAuth,
            [],
            RequestAuthorizer::request_time_bearer(credentials),
        )
        .unwrap();

        let error = provider
            .stream(test_request())
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(error, ProviderError::Api { status: 401, .. }));
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains(SECRET));

        let request = server.capture();
        assert_eq!(
            request.header("authorization"),
            Some("Bearer dynamic-chat-test-secret")
        );
    }

    struct StaticBearerCredential(&'static str);

    impl crate::RequestCredentialProvider for StaticBearerCredential {
        fn credential(&self) -> crate::RequestCredentialFuture<'_> {
            Box::pin(async { crate::RequestCredential::bearer(self.0) })
        }
    }

    #[tokio::test]
    async fn rejects_non_sse_success_responses() {
        let server = LoopbackServer::respond(200, "application/json", "{}");
        let endpoint = format!("{}/v1/chat/completions", server.base_url);
        let provider =
            OpenAiChatCompletions::with_endpoint(&endpoint, ChatCompletionsAuth::NoAuth, [], true)
                .unwrap();
        let error = provider
            .stream(test_request())
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(error, ProviderError::Protocol(_)));
        server.capture();
    }

    #[tokio::test]
    async fn reports_a_stream_that_ends_before_done() {
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
        let server = LoopbackServer::sse(body);
        let endpoint = format!("{}/v1/chat/completions", server.base_url);
        let provider =
            OpenAiChatCompletions::with_endpoint(&endpoint, ChatCompletionsAuth::NoAuth, [], true)
                .unwrap();
        let events = provider.stream(test_request()).collect::<Vec<_>>().await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::OutputTextDelta { text }) if text == "partial"
        ));
        assert!(matches!(&events[1], Err(ProviderError::Protocol(_))));
        server.capture();
    }

    fn test_request() -> ModelRequest {
        ModelRequest::new("chat-test", vec![Message::user("ping")], 128)
    }
}
