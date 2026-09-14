//! Anthropic Messages API adapter.

use std::sync::Arc;

use async_stream::try_stream;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName, HeaderValue};
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
    providers::support::{self, Text, ToolCallLedger, UsageOnce, value_as_status},
    request_auth::RequestAuthorizer,
    sanitize::sanitize_message,
    sse::{SseDecoder, SseEvent, Utf8ErrorMessage},
};

#[cfg(test)]
use crate::http::validate_endpoint;

#[cfg(test)]
const MESSAGES_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const DEFAULT_ANTHROPIC_VERSION: &str = "2023-06-01";
const X_API_KEY: HeaderName = HeaderName::from_static("x-api-key");
const ANTHROPIC_VERSION: HeaderName = HeaderName::from_static("anthropic-version");

const SSE_SPEC: SseExchangeSpec = SseExchangeSpec {
    messages: ExchangeMessages {
        wire_overflow: "Anthropic-compatible wire size overflowed",
        wire_limit: "Anthropic-compatible stream exceeded the configured wire size limit",
    },
    non_sse_response: "Anthropic-compatible provider returned a non-SSE response",
    content_type_gate: ContentTypeGate::Strict,
};

/// Authentication applied by an Anthropic-compatible Messages client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AnthropicAuth {
    NoAuth,
    XApiKey(SecretLiteral),
    Bearer(SecretLiteral),
    Header(String, SecretLiteral),
}

/// A client for Anthropic-compatible Messages endpoints.
pub(crate) struct AnthropicMessages {
    pub(crate) exchange: HttpExchange,
    endpoint: reqwest::Url,
    headers: HeaderMap,
}

#[cfg(test)]
impl AnthropicMessages {
    /// One attempt per request, so a scripted single-response server is
    /// observed exactly once.
    fn single_shot(mut self) -> Self {
        self.exchange
            .set_attempt_policy(crate::http::AttemptPolicy::disabled());
        self
    }
}

impl AnthropicMessages {
    /// Creates a client for Anthropic's standard Messages endpoint.
    #[cfg(test)]
    pub(crate) fn new(api_key: &str) -> Result<Self, ProviderError> {
        Self::with_endpoint(
            MESSAGES_ENDPOINT,
            AnthropicAuth::XApiKey(api_key.into()),
            [],
            false,
        )
    }

    /// Creates a client for an exact Anthropic-compatible endpoint URL.
    ///
    /// Plain HTTP is accepted only when `allow_http` is true and the URL host is
    /// loopback. The Anthropic version defaults to `2023-06-01`.
    #[cfg(test)]
    pub(crate) fn with_endpoint(
        endpoint: &str,
        auth: AnthropicAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        allow_http: bool,
    ) -> Result<Self, ProviderError> {
        Self::with_endpoint_and_version(
            endpoint,
            auth,
            static_headers,
            allow_http,
            DEFAULT_ANTHROPIC_VERSION,
        )
    }

    /// Creates a client with an explicit `anthropic-version` header value.
    #[cfg(test)]
    pub(crate) fn with_endpoint_and_version(
        endpoint: &str,
        auth: AnthropicAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        allow_http: bool,
        anthropic_version: &str,
    ) -> Result<Self, ProviderError> {
        let endpoint = validate_endpoint(endpoint, allow_http)?;
        let client = support::client_for_endpoint(&endpoint)?;
        Self::with_client_authorizer_and_version(
            client,
            endpoint,
            auth,
            static_headers,
            RequestAuthorizer::default(),
            anthropic_version,
        )
        .map(Self::single_shot)
    }

    pub(crate) fn with_client_and_authorizer(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        auth: AnthropicAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        authorizer: RequestAuthorizer,
    ) -> Result<Self, ProviderError> {
        Self::with_client_authorizer_and_version(
            client,
            endpoint,
            auth,
            static_headers,
            authorizer,
            DEFAULT_ANTHROPIC_VERSION,
        )
    }

    fn with_client_authorizer_and_version(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        auth: AnthropicAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
        authorizer: RequestAuthorizer,
        anthropic_version: &str,
    ) -> Result<Self, ProviderError> {
        let (headers, redactions) = build_headers(auth, static_headers, anthropic_version)?;

        Ok(Self {
            exchange: HttpExchange::new(client, authorizer, Arc::from(redactions)),
            endpoint,
            headers,
        })
    }
}

impl Provider for AnthropicMessages {
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
                let body = MessagesRequest::from(&request);
                let mut sse = sse_exchange(
                    &exchange,
                    (endpoint, headers),
                    &body,
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
                    "Anthropic-compatible output size overflowed",
                    "Anthropic-compatible output exceeded the configured size limit",
                );
                let mut usage = UsageOnce::new(
                    "Anthropic-compatible stream reported starting usage more than once",
                );
                // Maps streamed content-block indexes to tool-call ids so argument
                // deltas and block stops can be attributed after the start event.
                let mut tool_calls = ToolCallLedger::new(
                    "Anthropic-compatible stream reused a tool content-block index",
                    "Anthropic-compatible stream sent arguments for an unknown tool call",
                );
                let mut reasoning_blocks = std::collections::HashSet::new();
                let mut incomplete = None;

                while let Some(event) = sse.next_event().await? {
                    match decode_event(event, redactions.as_ref())? {
                        DecodedEvent::OutputText(text) => {
                            if text.is_empty() {
                                continue;
                            }
                            output_bytes.add(text.len())?;
                            yield ProviderEvent::OutputTextDelta { text };
                        }
                        DecodedEvent::MessageStart(start) => {
                            if let Some(start) = start {
                                usage.set(start)?;
                            }
                        }
                        DecodedEvent::MessageDelta { refusal, output_tokens, incomplete: reason } => {
                            if let Some(reason) = reason {
                                incomplete = Some(reason);
                            }
                            if let Some(text) = refusal {
                                output_bytes.add(text.len())?;
                                yield ProviderEvent::RefusalDelta { text };
                            }
                            if let Some(output_tokens) = output_tokens {
                                let current = usage.stored_mut().ok_or_else(|| {
                                    ProviderError::Protocol(
                                        "Anthropic-compatible stream reported output usage before starting usage".to_owned(),
                                    )
                                })?;
                                if output_tokens < current.output_tokens {
                                    Err(ProviderError::Protocol(
                                        "Anthropic-compatible cumulative output usage decreased".to_owned(),
                                    ))?;
                                }
                                current.output_tokens = output_tokens;
                            }
                        }
                        DecodedEvent::ToolCallStarted { index, id, name } => {
                            tool_calls.insert(index, id.clone())?;
                            yield ProviderEvent::ToolCallStarted { id, name };
                        }
                        DecodedEvent::ToolCallArguments { index, json } => {
                            let id = tool_calls.get(&index)?.to_owned();
                            output_bytes.add(json.len())?;
                            yield ProviderEvent::ToolCallArgumentsDelta { id, json };
                        }
                        DecodedEvent::ThinkingStarted { index } => {
                            if !reasoning_blocks.insert(index) {
                                Err(ProviderError::Protocol(
                                    "Anthropic-compatible stream reused a thinking content-block index"
                                        .to_owned(),
                                ))?;
                            }
                            yield ProviderEvent::ReasoningStarted {
                                kind: crate::ReasoningKind::ExposedThinking,
                            };
                        }
                        DecodedEvent::ThinkingDelta { index, text } => {
                            if !reasoning_blocks.contains(&index) {
                                Err(ProviderError::Protocol(
                                    "Anthropic-compatible stream sent thinking for an unknown block"
                                        .to_owned(),
                                ))?;
                            }
                            if !text.is_empty() {
                                output_bytes.add(text.len())?;
                                yield ProviderEvent::ReasoningDelta {
                                    kind: crate::ReasoningKind::ExposedThinking,
                                    text,
                                };
                            }
                        }
                        DecodedEvent::BlockStopped { index } => {
                            if reasoning_blocks.remove(&index) {
                                yield ProviderEvent::ReasoningCompleted {
                                    kind: crate::ReasoningKind::ExposedThinking,
                                };
                            } else if let Some(id) = tool_calls.remove(&index) {
                                yield ProviderEvent::ToolCallCompleted { id };
                            }
                        }
                        DecodedEvent::Completed => {
                            let usage = usage.finish();
                            match incomplete {
                                Some(reason) => yield ProviderEvent::Incomplete { usage, reason },
                                None => yield ProviderEvent::Completed { usage },
                            }
                            return;
                        }
                        DecodedEvent::Ignored => {}
                    }
                }

                Err(sse.ended_early("Anthropic-compatible stream ended before message_stop"))?;
            })
        })
    }
}

fn build_headers(
    auth: AnthropicAuth,
    static_headers: impl IntoIterator<Item = (String, String)>,
    anthropic_version: &str,
) -> Result<(HeaderMap, Vec<String>), ProviderError> {
    if anthropic_version.trim().is_empty() {
        return Err(ProviderError::Configuration(
            "anthropic-version must not be empty".to_owned(),
        ));
    }
    let version = HeaderValue::from_str(anthropic_version).map_err(|_| {
        ProviderError::Configuration(
            "anthropic-version is not a valid HTTP header value".to_owned(),
        )
    })?;

    let mut redactions = vec![anthropic_version.to_owned()];
    let auth_header = match auth {
        AnthropicAuth::NoAuth => None,
        AnthropicAuth::XApiKey(secret) => {
            let value = sensitive_header_value(&secret, "x-api-key secret")?;
            redactions.push(secret.expose_secret().to_owned());
            Some((X_API_KEY, value))
        }
        AnthropicAuth::Bearer(secret) => {
            let value = sensitive_bearer_value(&secret, "Bearer secret")?;
            redactions.push(secret.expose_secret().to_owned());
            Some((AUTHORIZATION, value))
        }
        AnthropicAuth::Header(name, secret) => {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ProviderError::Configuration("authentication header name is invalid".to_owned())
            })?;
            if name == ANTHROPIC_VERSION || is_request_controlled_header(&name) {
                return Err(ProviderError::Configuration(
                    "authentication header is controlled by the provider".to_owned(),
                ));
            }
            let value = sensitive_header_value(&secret, "authentication header secret")?;
            redactions.push(secret.expose_secret().to_owned());
            Some((name, value))
        }
    };
    let auth_name = auth_header.as_ref().map(|(name, _)| name.clone());
    let mut headers = SafeHeaders::new(
        [AUTHORIZATION, X_API_KEY, ANTHROPIC_VERSION]
            .into_iter()
            .chain(auth_name),
    );
    headers.insert_configured(static_headers, false)?;
    headers.insert_owned(ANTHROPIC_VERSION, version);
    if let Some((name, value)) = auth_header {
        headers.insert_owned(name, value);
    }
    for redaction in redactions {
        headers.push_redaction(redaction);
    }
    Ok(headers.finish())
}

fn sse_decoder(max_event_bytes: usize) -> SseDecoder {
    SseDecoder::named(
        max_event_bytes,
        "Anthropic-compatible SSE event size overflowed",
        "Anthropic-compatible SSE event exceeded the configured size limit",
        Utf8ErrorMessage::Static("Anthropic-compatible SSE event data was not UTF-8"),
        Utf8ErrorMessage::Static("Anthropic-compatible SSE event name was not UTF-8"),
    )
}

#[derive(Serialize)]
pub(crate) struct MessagesRequest<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Text<'a>>,
    messages: Vec<AnthropicMessage<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool<'a>>,
    max_tokens: u32,
    stream: bool,
}

impl<'a> From<&'a ModelRequest> for MessagesRequest<'a> {
    fn from(request: &'a ModelRequest) -> Self {
        Self {
            model: request.model(),
            system: request.system().map(Text),
            messages: request
                .messages()
                .iter()
                .map(AnthropicMessage::from)
                .collect(),
            tools: request.tools().iter().map(AnthropicTool::from).collect(),
            max_tokens: request.max_output_tokens(),
            stream: true,
        }
    }
}

#[derive(Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: &'a RawValue,
}

impl<'a> From<&'a ToolSpec> for AnthropicTool<'a> {
    fn from(tool: &'a ToolSpec) -> Self {
        Self {
            name: tool.name(),
            description: tool.description(),
            input_schema: tool.input_schema(),
        }
    }
}

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: AnthropicRole,
    content: AnthropicContent<'a>,
}

impl<'a> From<&'a Message> for AnthropicMessage<'a> {
    fn from(message: &'a Message) -> Self {
        // A single text block serializes as a plain string so tool-less
        // requests keep their existing wire shape.
        let content = match message.content() {
            [ContentBlock::Text { text }] => AnthropicContent::Text(Text(text)),
            blocks => AnthropicContent::Blocks(blocks.iter().map(AnthropicBlock::from).collect()),
        };
        Self {
            role: match message.role() {
                Role::User => AnthropicRole::User,
                Role::Assistant => AnthropicRole::Assistant,
            },
            content,
        }
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum AnthropicContent<'a> {
    Text(Text<'a>),
    Blocks(Vec<AnthropicBlock<'a>>),
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum AnthropicBlock<'a> {
    Text {
        text: Text<'a>,
    },
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a RawValue,
    },
    ToolResult {
        tool_use_id: &'a str,
        content: Text<'a>,
        is_error: bool,
    },
}

impl<'a> From<&'a ContentBlock> for AnthropicBlock<'a> {
    fn from(block: &'a ContentBlock) -> Self {
        match block {
            ContentBlock::Text { text } => Self::Text { text: Text(text) },
            ContentBlock::ToolCall {
                id,
                name,
                arguments,
            } => Self::ToolUse {
                id,
                name,
                input: arguments,
            },
            ContentBlock::ToolResult {
                call_id,
                content,
                is_error,
            } => Self::ToolResult {
                tool_use_id: call_id,
                content: Text(content),
                is_error: *is_error,
            },
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum AnthropicRole {
    User,
    Assistant,
}

#[derive(Deserialize)]
struct EventEnvelope {
    #[serde(rename = "type")]
    event_type: String,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum StreamingEvent {
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u64, delta: ContentDelta },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: MessageDelta,
        usage: Option<MessageDeltaUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "error")]
    Error { error: WireApiError },
    #[serde(rename = "message_start")]
    MessageStart { message: StartedMessage },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: u64,
        content_block: StartedBlock,
    },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u64 },
    #[serde(rename = "ping")]
    Ping,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum StartedBlock {
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(rename = "thinking")]
    Thinking,
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ContentDelta {
    #[serde(rename = "text_delta")]
    Text { text: String },
    #[serde(rename = "input_json_delta")]
    InputJson { partial_json: String },
    #[serde(rename = "thinking_delta")]
    Thinking { thinking: String },
    #[serde(other)]
    Other,
}

#[derive(Deserialize)]
struct MessageDelta {
    stop_reason: Option<String>,
    stop_details: Option<StopDetails>,
}

#[derive(Deserialize)]
struct MessageDeltaUsage {
    output_tokens: u64,
}

#[derive(Deserialize)]
struct StartedMessage {
    usage: Option<AnthropicUsage>,
}

#[derive(Deserialize)]
struct AnthropicUsage {
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

#[derive(Deserialize)]
struct StopDetails {
    #[serde(rename = "type")]
    detail_type: Option<String>,
    explanation: Option<String>,
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

#[derive(Debug, PartialEq, Eq)]
enum DecodedEvent {
    OutputText(String),
    MessageStart(Option<ProviderUsage>),
    MessageDelta {
        refusal: Option<String>,
        output_tokens: Option<u64>,
        /// A recoverable stop reason; the stream still runs to
        /// `message_stop` so the terminal event carries final usage.
        incomplete: Option<IncompleteReason>,
    },
    ThinkingStarted {
        index: u64,
    },
    ThinkingDelta {
        index: u64,
        text: String,
    },
    ToolCallStarted {
        index: u64,
        id: String,
        name: String,
    },
    ToolCallArguments {
        index: u64,
        json: String,
    },
    BlockStopped {
        index: u64,
    },
    Completed,
    Ignored,
}

fn decode_event(event: SseEvent, redactions: &[String]) -> Result<DecodedEvent, ProviderError> {
    if event.data.trim().is_empty() {
        return Ok(DecodedEvent::Ignored);
    }

    let envelope: EventEnvelope = serde_json::from_str(&event.data).map_err(|error| {
        ProviderError::Protocol(sanitize_message(
            &format!("could not decode Anthropic-compatible event envelope: {error}"),
            redactions,
        ))
    })?;
    if event
        .name
        .as_deref()
        .is_some_and(|name| name != envelope.event_type)
    {
        return Err(ProviderError::Protocol(
            "Anthropic-compatible SSE event name did not match its payload type".to_owned(),
        ));
    }

    let event: StreamingEvent = serde_json::from_str(&event.data).map_err(|error| {
        ProviderError::Protocol(sanitize_message(
            &format!("could not decode Anthropic-compatible event: {error}"),
            redactions,
        ))
    })?;

    match event {
        StreamingEvent::ContentBlockDelta {
            delta: ContentDelta::Text { text },
            ..
        } => Ok(DecodedEvent::OutputText(text)),
        StreamingEvent::ContentBlockDelta {
            index,
            delta: ContentDelta::InputJson { partial_json },
        } => Ok(DecodedEvent::ToolCallArguments {
            index,
            json: partial_json,
        }),
        StreamingEvent::ContentBlockDelta {
            index,
            delta: ContentDelta::Thinking { thinking },
        } => Ok(DecodedEvent::ThinkingDelta {
            index,
            text: thinking,
        }),
        StreamingEvent::ContentBlockStart {
            index,
            content_block: StartedBlock::Thinking,
        } => Ok(DecodedEvent::ThinkingStarted { index }),
        StreamingEvent::ContentBlockStart {
            index,
            content_block: StartedBlock::ToolUse { id, name },
        } => Ok(DecodedEvent::ToolCallStarted { index, id, name }),
        StreamingEvent::ContentBlockStop { index } => Ok(DecodedEvent::BlockStopped { index }),
        StreamingEvent::ContentBlockDelta {
            delta: ContentDelta::Other,
            ..
        }
        | StreamingEvent::ContentBlockStart {
            content_block: StartedBlock::Other,
            ..
        }
        | StreamingEvent::Ping
        | StreamingEvent::Other => Ok(DecodedEvent::Ignored),
        StreamingEvent::MessageStart { message } => {
            Ok(DecodedEvent::MessageStart(message.usage.map(|usage| {
                ProviderUsage {
                    input_tokens: usage.input_tokens,
                    cache_read_input_tokens: usage.cache_read_input_tokens,
                    cache_write_input_tokens: usage.cache_creation_input_tokens,
                    output_tokens: usage.output_tokens,
                    // Anthropic bills thinking inside output_tokens and does
                    // not break it out.
                    reasoning_tokens: None,
                }
            })))
        }
        StreamingEvent::MessageDelta { delta, usage } => {
            let (refusal, incomplete) = decode_message_delta(delta, redactions)?;
            Ok(DecodedEvent::MessageDelta {
                refusal,
                output_tokens: usage.map(|usage| usage.output_tokens),
                incomplete,
            })
        }
        StreamingEvent::MessageStop => Ok(DecodedEvent::Completed),
        StreamingEvent::Error { error } => Err(wire_api_error(error, redactions)),
    }
}

/// Decodes a `message_delta` stop into the refusal text (if the model
/// refused) and the recoverable incomplete reason (if it stopped short).
fn decode_message_delta(
    delta: MessageDelta,
    redactions: &[String],
) -> Result<(Option<String>, Option<IncompleteReason>), ProviderError> {
    let details_are_refusal = delta
        .stop_details
        .as_ref()
        .and_then(|details| details.detail_type.as_deref())
        == Some("refusal");
    if delta.stop_reason.as_deref() == Some("refusal") || details_are_refusal {
        let explanation = delta
            .stop_details
            .and_then(|details| details.explanation)
            .filter(|explanation| !explanation.trim().is_empty())
            .map_or_else(
                || "Anthropic declined the request".to_owned(),
                |explanation| sanitize_message(&explanation, redactions),
            );
        return Ok((Some(explanation), None));
    }

    match delta.stop_reason.as_deref() {
        None | Some("end_turn" | "stop_sequence" | "tool_use") => Ok((None, None)),
        Some("max_tokens") => Ok((None, Some(IncompleteReason::OutputTokens))),
        Some("pause_turn") => Ok((None, Some(IncompleteReason::Paused))),
        Some("model_context_window_exceeded") => Err(ProviderError::ResponseFailed {
            kind: ProviderErrorKind::ContextExceeded,
            message: "Anthropic request exceeded the model context window".to_owned(),
        }),
        Some(_) => Err(ProviderError::Protocol(
            "Anthropic response used an unsupported stop reason".to_owned(),
        )),
    }
}

fn wire_api_error(error: WireApiError, redactions: &[String]) -> ProviderError {
    let kind = wire_error_kind(&error);
    let message = error.message.as_deref().map_or_else(
        || "Anthropic-compatible provider did not provide an error message".to_owned(),
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

fn status_error_kind(status: u16) -> ProviderErrorKind {
    // Anthropic classifies 413 (request too large) as a context-size
    // failure the session layer can compact away.
    match status {
        413 => ProviderErrorKind::ContextExceeded,
        status => support::status_error_kind(status),
    }
}

fn named_error_kind(name: &str) -> ProviderErrorKind {
    match name.to_ascii_lowercase().as_str() {
        "authentication_error" | "invalid_api_key" | "permission_error" => {
            ProviderErrorKind::Authentication
        }
        "rate_limit_error" | "rate_limit_exceeded" => ProviderErrorKind::RateLimited,
        "conflict_error" | "invalid_request_error" | "model_not_found" | "not_found_error" => {
            ProviderErrorKind::InvalidRequest
        }
        "request_too_large" => ProviderErrorKind::ContextExceeded,
        "api_error" | "overloaded_error" | "service_unavailable" | "timeout_error" => {
            ProviderErrorKind::Unavailable
        }
        _ => ProviderErrorKind::Response,
    }
}

fn api_error(rejection: HttpRejection) -> ProviderError {
    support::api_error(
        rejection,
        "Anthropic-compatible request failed",
        |envelope: ApiErrorEnvelope| envelope.error.message,
    )
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use serde_json::json;

    use crate::test_support::LoopbackServer;

    use super::*;

    #[test]
    fn default_constructor_uses_anthropic_endpoint_and_redacts_auth_debug() {
        let provider = AnthropicMessages::new("anthropic-test-secret").unwrap();
        let auth_values = [
            AnthropicAuth::XApiKey("anthropic-test-secret".into()),
            AnthropicAuth::Bearer("anthropic-test-secret".into()),
            AnthropicAuth::Header("x-custom-auth".to_owned(), "anthropic-test-secret".into()),
        ];

        assert_eq!(provider.endpoint.as_str(), MESSAGES_ENDPOINT);
        for auth in auth_values {
            assert!(!format!("{auth:?}").contains("anthropic-test-secret"));
        }
    }

    #[test]
    fn validates_http_endpoint_policy_and_url_components() {
        for (endpoint, allow_http) in [
            ("not a URL", false),
            ("http://example.com/v1/messages", true),
            ("http://127.0.0.1/v1/messages", false),
            ("https://user:password@example.com/v1/messages", false),
            ("https://example.com/v1/messages#fragment", false),
            ("ftp://example.com/v1/messages", false),
        ] {
            let error =
                AnthropicMessages::with_endpoint(endpoint, AnthropicAuth::NoAuth, [], allow_http)
                    .err()
                    .expect("endpoint must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }

        AnthropicMessages::with_endpoint(
            "http://[::1]/v1/messages",
            AnthropicAuth::NoAuth,
            [],
            true,
        )
        .expect("IPv6 loopback HTTP should be accepted");
        AnthropicMessages::with_endpoint(
            "http://localhost/v1/messages",
            AnthropicAuth::NoAuth,
            [],
            true,
        )
        .expect("localhost HTTP should be accepted");
    }

    #[test]
    fn rejects_controlled_duplicate_and_invalid_headers() {
        for name in [
            "authorization",
            "x-api-key",
            "anthropic-version",
            "host",
            "content-length",
            "connection",
            "transfer-encoding",
            "accept",
            "content-type",
            "user-agent",
        ] {
            let error = AnthropicMessages::with_endpoint(
                "https://example.com/v1/messages",
                AnthropicAuth::NoAuth,
                [(name.to_owned(), "value".to_owned())],
                false,
            )
            .err()
            .expect("controlled header must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }

        for (auth, headers) in [
            (
                AnthropicAuth::Header("x-custom-auth".to_owned(), "secret".into()),
                vec![("x-custom-auth".to_owned(), "override".to_owned())],
            ),
            (
                AnthropicAuth::NoAuth,
                vec![
                    ("x-test".to_owned(), "one".to_owned()),
                    ("X-Test".to_owned(), "two".to_owned()),
                ],
            ),
            (
                AnthropicAuth::NoAuth,
                vec![("bad header".to_owned(), "value".to_owned())],
            ),
            (
                AnthropicAuth::NoAuth,
                vec![("x-test".to_owned(), "bad\r\nvalue".to_owned())],
            ),
        ] {
            let error = AnthropicMessages::with_endpoint(
                "https://example.com/v1/messages",
                auth,
                headers,
                false,
            )
            .err()
            .expect("invalid headers must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }

        for auth in [
            AnthropicAuth::XApiKey("".into()),
            AnthropicAuth::Bearer("".into()),
            AnthropicAuth::Header("anthropic-version".to_owned(), "secret".into()),
            AnthropicAuth::Header("x-auth".to_owned(), "".into()),
        ] {
            let error = AnthropicMessages::with_endpoint(
                "https://example.com/v1/messages",
                auth,
                [],
                false,
            )
            .err()
            .expect("invalid authentication must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }
    }

    #[test]
    fn validates_and_applies_an_explicit_anthropic_version() {
        let (headers, redactions) =
            build_headers(AnthropicAuth::NoAuth, [], "mantle-version-test-secret").unwrap();
        assert_eq!(
            headers.get(ANTHROPIC_VERSION).unwrap(),
            "mantle-version-test-secret"
        );
        assert!(
            redactions
                .iter()
                .any(|value| value == "mantle-version-test-secret")
        );

        for version in ["", "bad\r\nversion"] {
            let error = AnthropicMessages::with_endpoint_and_version(
                "https://example.com/v1/messages",
                AnthropicAuth::NoAuth,
                [],
                false,
                version,
            )
            .err()
            .expect("invalid version must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }
    }

    #[test]
    fn decodes_fragmented_named_sse_with_bom_and_all_line_endings() {
        let source = concat!(
            "\u{feff}: comment\r\n",
            "event: content_block_delta\r",
            "data: {\"type\":\"content_block_delta\",\r\n",
            "data: \"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hello\"}}\n\r",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\r\r",
        );
        let mut decoder = sse_decoder(1_024);
        let mut events = Vec::new();

        for byte in source.as_bytes() {
            events.extend(decoder.push(std::slice::from_ref(byte)).unwrap());
        }

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name.as_deref(), Some("content_block_delta"));
        assert_eq!(events[1].name.as_deref(), Some("message_stop"));
        assert!(matches!(
            decode_event(events.remove(0), &[]).unwrap(),
            DecodedEvent::OutputText(text) if text == "hello"
        ));
        assert!(matches!(
            decode_event(events.remove(0), &[]).unwrap(),
            DecodedEvent::Completed
        ));
    }

    #[test]
    fn decodes_cumulative_usage_and_rejects_overflow() {
        let start = decode_data(
            "message_start",
            r#"{"type":"message_start","message":{"usage":{"input_tokens":12,"cache_creation_input_tokens":3,"cache_read_input_tokens":4,"output_tokens":1}}}"#,
        )
        .unwrap();
        let delta = decode_event(
            SseEvent {
                name: Some("message_delta".to_owned()),
                data: r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":9}}"#.to_owned(),
            },
            &[],
        )
        .unwrap();

        assert_eq!(
            start,
            DecodedEvent::MessageStart(Some(ProviderUsage {
                input_tokens: 12,
                cache_read_input_tokens: 4,
                cache_write_input_tokens: 3,
                output_tokens: 1,
                reasoning_tokens: None,
            }))
        );
        assert_eq!(
            delta,
            DecodedEvent::MessageDelta {
                refusal: None,
                output_tokens: Some(9),
                incomplete: None,
            }
        );

        let error = decode_data(
            "message_start",
            r#"{"type":"message_start","message":{"usage":{"input_tokens":18446744073709551616,"output_tokens":1}}}"#,
        )
        .unwrap_err();
        assert!(matches!(error, ProviderError::Protocol(_)));
    }

    #[test]
    fn decodes_displayable_thinking_but_ignores_redacted_thinking() {
        let started = decode_data(
            "content_block_start",
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"thinking","thinking":""}}"#,
        )
        .unwrap();
        let delta = decode_data(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"thinking_delta","thinking":"checking"}}"#,
        )
        .unwrap();
        let redacted = decode_data(
            "content_block_start",
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"redacted_thinking","data":"opaque-secret-data"}}"#,
        )
        .unwrap();

        assert_eq!(started, DecodedEvent::ThinkingStarted { index: 2 });
        assert_eq!(
            delta,
            DecodedEvent::ThinkingDelta {
                index: 2,
                text: "checking".to_owned(),
            }
        );
        assert_eq!(redacted, DecodedEvent::Ignored);
    }

    #[test]
    fn handles_text_refusal_errors_and_opaque_events() {
        let text = decode_data(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}"#,
        )
        .unwrap();
        let refusal = decode_data(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"type":"refusal","category":"cyber","explanation":"request declined"}}}"#,
        )
        .unwrap();
        let thinking = decode_data(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"private reasoning"}}"#,
        )
        .unwrap();
        let redacted = decode_data(
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque-secret-data"}}"#,
        )
        .unwrap();
        let overloaded = decode_data(
            "error",
            r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#,
        )
        .unwrap_err();
        let rate_limited = decode_data(
            "error",
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
        )
        .unwrap_err();

        assert!(matches!(text, DecodedEvent::OutputText(text) if text == "hello"));
        assert!(matches!(
            refusal,
            DecodedEvent::MessageDelta {
                refusal: Some(text),
                output_tokens: None,
                incomplete: None,
            } if text == "request declined"
        ));
        assert_eq!(
            thinking,
            DecodedEvent::ThinkingDelta {
                index: 0,
                text: "private reasoning".to_owned(),
            }
        );
        assert_eq!(redacted, DecodedEvent::Ignored);
        assert_eq!(overloaded.kind(), ProviderErrorKind::Unavailable);
        assert_eq!(rate_limited.kind(), ProviderErrorKind::RateLimited);
    }

    #[test]
    fn classifies_incomplete_and_unsupported_stop_reasons() {
        let max_tokens = decode_data(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":64}}"#,
        )
        .unwrap();
        let paused = decode_data(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"pause_turn"}}"#,
        )
        .unwrap();
        let unknown = decode_data(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"mystery"}}"#,
        )
        .unwrap_err();
        let tool_use = decode_data(
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        )
        .unwrap();

        assert_eq!(
            max_tokens,
            DecodedEvent::MessageDelta {
                refusal: None,
                output_tokens: Some(64),
                incomplete: Some(IncompleteReason::OutputTokens),
            }
        );
        assert_eq!(
            paused,
            DecodedEvent::MessageDelta {
                refusal: None,
                output_tokens: None,
                incomplete: Some(IncompleteReason::Paused),
            }
        );
        assert!(matches!(unknown, ProviderError::Protocol(_)));
        assert_eq!(
            tool_use,
            DecodedEvent::MessageDelta {
                refusal: None,
                output_tokens: None,
                incomplete: None,
            }
        );
    }

    #[test]
    fn decodes_tool_call_stream_events() {
        let started = decode_data(
            "content_block_start",
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read_file","input":{}}}"#,
        )
        .unwrap();
        let arguments = decode_data(
            "content_block_delta",
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}"#,
        )
        .unwrap();
        let stopped = decode_data(
            "content_block_stop",
            r#"{"type":"content_block_stop","index":1}"#,
        )
        .unwrap();

        assert_eq!(
            started,
            DecodedEvent::ToolCallStarted {
                index: 1,
                id: "toolu_1".to_owned(),
                name: "read_file".to_owned(),
            }
        );
        assert_eq!(
            arguments,
            DecodedEvent::ToolCallArguments {
                index: 1,
                json: "{\"path\":".to_owned(),
            }
        );
        assert_eq!(stopped, DecodedEvent::BlockStopped { index: 1 });
    }

    #[test]
    fn rejects_mismatched_event_names_and_enforces_all_size_limits() {
        let mismatch = decode_data("message_stop", r#"{"type":"ping"}"#).unwrap_err();
        let event_error = sse_decoder(8)
            .push(b"data: this event keeps going")
            .unwrap_err();
        let mut output = ByteCounter::new(4, "output overflow", "output limit");
        let output_error = output.add(5).unwrap_err();
        let mut wire = ByteCounter::new(8, "wire overflow", "wire limit");
        let wire_error = wire.add(9).unwrap_err();

        assert!(matches!(mismatch, ProviderError::Protocol(_)));
        assert!(matches!(event_error, ProviderError::Protocol(_)));
        assert!(matches!(output_error, ProviderError::Protocol(_)));
        assert!(matches!(wire_error, ProviderError::Protocol(_)));
    }

    #[tokio::test]
    async fn sends_exact_request_and_streams_fragmented_text_to_completion() {
        let chunks = vec![
            b"\xef".to_vec(),
            b"\xbb".to_vec(),
            b"\xbf: heartbeat\r".to_vec(),
            b"\nevent: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":12,\"cache_creation_input_tokens\":3,\"cache_read_input_tokens\":4,\"output_tokens\":1}}}\r\n\r"
                .to_vec(),
            b"\nevent: ping\ndata: {\"type\":\"ping\"}\n\n".to_vec(),
            b"event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"h\xc3"
                .to_vec(),
            b"\xa9l\"}}\r\n\r\n".to_vec(),
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"ignored\"}}\n\n"
                .to_vec(),
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n"
                .to_vec(),
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":9}}\n\n"
                .to_vec(),
            b"event: message_stop\ndata: {\"type\":\"message_".to_vec(),
            b"stop\"}\r\r".to_vec(),
        ];
        let server =
            LoopbackServer::respond_chunks(200, Some("text/event-stream; charset=utf-8"), chunks);
        let endpoint = format!("{}/custom/messages?api-version=42", server.base_url);
        let provider = AnthropicMessages::with_endpoint(
            &endpoint,
            AnthropicAuth::XApiKey("custom-test-secret".into()),
            [("x-client".to_owned(), "qq-tests".to_owned())],
            true,
        )
        .unwrap();
        let events = provider
            .stream(ModelRequest::new(
                "claude-test",
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
            Ok(ProviderEvent::OutputTextDelta { text }) if text == "lo"
        ));
        assert_eq!(
            events[2].as_ref().unwrap(),
            &ProviderEvent::Completed {
                usage: Some(ProviderUsage {
                    input_tokens: 12,
                    cache_read_input_tokens: 4,
                    cache_write_input_tokens: 3,
                    output_tokens: 9,
                    reasoning_tokens: None,
                }),
            }
        );

        let request = server.capture();
        assert_eq!(
            request.request_line(),
            Some("POST /custom/messages?api-version=42 HTTP/1.1")
        );
        assert_eq!(request.header("accept"), Some("text/event-stream"));
        assert_eq!(request.header("content-type"), Some("application/json"));
        assert_eq!(request.header("x-api-key"), Some("custom-test-secret"));
        assert_eq!(
            request.header("anthropic-version"),
            Some(DEFAULT_ANTHROPIC_VERSION)
        );
        assert_eq!(request.header("x-client"), Some("qq-tests"));
        assert_eq!(request.header("authorization"), None);
        let body = request.json_body();
        assert_eq!(
            body,
            json!({
                "model": "claude-test",
                "messages": [
                    {"role": "user", "content": "ping"},
                    {"role": "assistant", "content": "pong"}
                ],
                "max_tokens": 321,
                "stream": true
            })
        );
        assert!(!body.as_object().unwrap().contains_key("system"));
    }

    #[tokio::test]
    async fn sends_tool_declarations_and_tool_history_blocks() {
        let body = concat!(
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let server = LoopbackServer::sse(body);
        let endpoint = format!("{}/v1/messages", server.base_url);
        let provider =
            AnthropicMessages::with_endpoint(&endpoint, AnthropicAuth::NoAuth, [], true).unwrap();
        let request = ModelRequest::new(
            "claude-test",
            vec![
                Message::user("read the config"),
                Message::new(
                    Role::Assistant,
                    vec![
                        ContentBlock::Text {
                            text: "Reading it now.".to_owned(),
                        },
                        ContentBlock::tool_call(
                            "toolu_1".to_owned(),
                            "read_file".to_owned(),
                            &json!({"path": "config.ron"}),
                        ),
                    ],
                ),
                Message::tool_results(vec![ContentBlock::ToolResult {
                    call_id: "toolu_1".to_owned(),
                    content: "(config)".to_owned(),
                    is_error: false,
                }]),
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
                "model": "claude-test",
                "messages": [
                    {"role": "user", "content": "read the config"},
                    {"role": "assistant", "content": [
                        {"type": "text", "text": "Reading it now."},
                        {"type": "tool_use", "id": "toolu_1", "name": "read_file",
                         "input": {"path": "config.ron"}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_1",
                         "content": "(config)", "is_error": false}
                    ]}
                ],
                "tools": [
                    {"name": "read_file", "description": "Reads one file",
                     "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}}
                ],
                "max_tokens": 128,
                "stream": true
            })
        );
    }

    #[test]
    fn maps_the_system_prompt_to_the_native_system_field() {
        let request = ModelRequest::new("claude-test", vec![Message::user("ping")], 64)
            .with_system("You are QQ.");
        let body = serde_json::to_value(MessagesRequest::from(&request)).unwrap();
        assert_eq!(body["system"], "You are QQ.");
        assert_eq!(body["messages"][0]["content"], "ping");

        let without = ModelRequest::new("claude-test", vec![Message::user("ping")], 64);
        let body = serde_json::to_value(MessagesRequest::from(&without)).unwrap();
        assert!(body.get("system").is_none());
    }

    #[tokio::test]
    async fn streams_tool_calls_with_attributed_arguments_to_completion() {
        let body = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Checking.\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"read_file\",\"input\":{}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\"}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"a.rs\\\"}\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let server = LoopbackServer::sse(body);
        let endpoint = format!("{}/v1/messages", server.base_url);
        let provider =
            AnthropicMessages::with_endpoint(&endpoint, AnthropicAuth::NoAuth, [], true).unwrap();
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
                    id: "toolu_1".to_owned(),
                    name: "read_file".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "toolu_1".to_owned(),
                    json: "{\"path\":".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "toolu_1".to_owned(),
                    json: "\"a.rs\"}".to_owned(),
                },
                ProviderEvent::ToolCallCompleted {
                    id: "toolu_1".to_owned(),
                },
                ProviderEvent::Completed { usage: None },
            ]
        );
        server.capture();
    }

    #[tokio::test]
    async fn rejects_tool_arguments_for_an_unknown_call() {
        let body = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":4,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\n",
        );
        let server = LoopbackServer::sse(body);
        let endpoint = format!("{}/v1/messages", server.base_url);
        let provider =
            AnthropicMessages::with_endpoint(&endpoint, AnthropicAuth::NoAuth, [], true).unwrap();
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
    async fn returns_typed_stream_errors_without_exposing_secrets() {
        let body = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",",
            "\"message\":\"stream-auth-secret static-test-secret overloaded\"}}\n\n",
        );
        let server = LoopbackServer::sse(body);
        let endpoint = format!("{}/v1/messages", server.base_url);
        let provider = AnthropicMessages::with_endpoint(
            &endpoint,
            AnthropicAuth::Bearer("stream-auth-secret".into()),
            [(
                "x-client-secret".to_owned(),
                "static-test-secret".to_owned(),
            )],
            true,
        )
        .unwrap();
        let error = provider
            .stream(test_request())
            .next()
            .await
            .unwrap()
            .unwrap_err();

        assert!(matches!(error, ProviderError::ResponseFailed { .. }));
        assert_eq!(error.kind(), ProviderErrorKind::Unavailable);
        let rendered = format!("{error:?} {error}");
        assert!(!rendered.contains("stream-auth-secret"));
        assert!(!rendered.contains("static-test-secret"));
        server.capture();
    }

    #[tokio::test]
    async fn returns_typed_401_without_exposing_response_body_secrets() {
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid test-api-secret\nstatic-test-secret credential"},"request_id":"req_test"}"#;
        let server = LoopbackServer::respond(401, "application/json", body);
        let endpoint = format!("{}/v1/messages", server.base_url);
        let provider = AnthropicMessages::with_endpoint(
            &endpoint,
            AnthropicAuth::XApiKey("test-api-secret".into()),
            [(
                "x-client-secret".to_owned(),
                "static-test-secret".to_owned(),
            )],
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
        assert!(!rendered.contains("static-test-secret"));
        assert!(!rendered.contains('\n'));

        let request = server.capture();
        assert_eq!(request.header("x-api-key"), Some("test-api-secret"));
    }

    #[tokio::test]
    async fn rejects_non_sse_success_responses() {
        let server = LoopbackServer::respond(200, "application/json", "{}");
        let endpoint = format!("{}/v1/messages", server.base_url);
        let provider =
            AnthropicMessages::with_endpoint(&endpoint, AnthropicAuth::NoAuth, [], true).unwrap();
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
    async fn reports_a_stream_that_ends_before_message_stop() {
        let body = concat!(
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,",
            "\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        );
        let server = LoopbackServer::sse(body);
        let endpoint = format!("{}/v1/messages", server.base_url);
        let provider =
            AnthropicMessages::with_endpoint(&endpoint, AnthropicAuth::NoAuth, [], true).unwrap();
        let events = provider.stream(test_request()).collect::<Vec<_>>().await;

        assert!(matches!(
            &events[0],
            Ok(ProviderEvent::OutputTextDelta { text }) if text == "partial"
        ));
        assert!(matches!(&events[1], Err(ProviderError::Protocol(_))));
        server.capture();
    }

    fn decode_data(name: &str, data: &str) -> Result<DecodedEvent, ProviderError> {
        decode_event(
            SseEvent {
                name: Some(name.to_owned()),
                data: data.to_owned(),
            },
            &[],
        )
    }

    fn test_request() -> ModelRequest {
        ModelRequest::new("claude-test", vec![Message::user("ping")], 128)
    }

    /// The provider is the single retry owner: a 503 before the body and an
    /// empty body before the first event each cost one attempt under the same
    /// policy, and the eventual stream is delivered once with no duplicates.
    #[tokio::test]
    async fn the_attempt_policy_covers_rejections_and_empty_bodies_alike() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            sync::atomic::{AtomicUsize, Ordering},
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1/messages", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = Arc::clone(&hits);
        let stream_body = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"pong\"}}\n\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let responses = [
            "HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\nContent-Length: 4\r\n\r\nbusy".to_owned(),
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/event-stream\r\nContent-Length: 0\r\n\r\n".to_owned(),
            format!(
                "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{stream_body}",
                stream_body.len()
            ),
        ];
        let server = std::thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buffer = [0_u8; 8192];
                let mut request = Vec::new();
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..read]);
                    if read == 0 || request.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                server_hits.fetch_add(1, Ordering::SeqCst);
                stream.write_all(response.as_bytes()).unwrap();
            }
        });

        let mut provider = AnthropicMessages::with_endpoint(
            &endpoint,
            AnthropicAuth::Bearer("secret".into()),
            [],
            true,
        )
        .unwrap();
        provider
            .exchange
            .set_attempt_policy(crate::http::AttemptPolicy::new(
                4,
                std::time::Duration::from_millis(1),
                std::time::Duration::from_millis(1),
                std::time::Duration::from_secs(5),
            ));

        let events: Vec<_> = provider.stream(test_request()).collect().await;
        server.join().unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 3, "one send per attempt");
        let texts: Vec<&str> = events
            .iter()
            .filter_map(|event| match event {
                Ok(ProviderEvent::OutputTextDelta { text }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(texts, ["pong"], "no duplicated output: {events:?}");
        assert!(matches!(
            events.last(),
            Some(Ok(ProviderEvent::Completed { .. }))
        ));
    }
}
