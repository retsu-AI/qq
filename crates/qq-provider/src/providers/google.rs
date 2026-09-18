//! Google Gemini GenerateContent API adapter.

use std::{collections::BTreeMap, sync::Arc};

use async_stream::try_stream;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderName};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, value::RawValue};

use crate::{
    ContentBlock, IncompleteReason, Message, ModelRequest, Provider, ProviderError,
    ProviderErrorKind, ProviderEvent, ProviderStream, ProviderUsage, Role, ToolSpec,
    compiler::EndpointKind,
    credentials::{SecretLiteral, sensitive_bearer_value, sensitive_header_value},
    exchange::{ContentTypeGate, SseExchangeSpec, sse_exchange, with_restart},
    http::{
        ExchangeMessages, HttpExchange, HttpRejection, SafeHeaders, is_request_controlled_header,
    },
    limits::{ByteCounter, StreamLimits},
    providers::support::{self, Text, UsageOnce, status_error_kind, subtract_cached_input_tokens},
    request_auth::RequestAuthorizer,
    sanitize::sanitize_message,
    sse::{SseDecoder, Utf8ErrorMessage},
};

#[cfg(test)]
use crate::http::validate_endpoint;
const X_GOOG_API_KEY: HeaderName = HeaderName::from_static("x-goog-api-key");

const SSE_SPEC: SseExchangeSpec = SseExchangeSpec {
    messages: ExchangeMessages {
        wire_overflow: "Google GenerateContent wire size overflowed",
        wire_limit: "Google GenerateContent stream exceeded the configured wire size limit",
    },
    non_sse_response: "Google GenerateContent provider returned a non-SSE response",
    content_type_gate: ContentTypeGate::Strict,
};

/// Authentication applied by a Google GenerateContent-compatible client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum GoogleAuth {
    NoAuth,
    XGoogApiKey(SecretLiteral),
    Bearer(SecretLiteral),
    Header(String, SecretLiteral),
}

/// A client for Google GenerateContent-compatible endpoints.
pub(crate) struct GoogleGenerateContent {
    pub(crate) exchange: HttpExchange,
    endpoint: reqwest::Url,
    endpoint_kind: EndpointKind,
    headers: Arc<HeaderMap>,
}

#[cfg(test)]
impl GoogleGenerateContent {
    /// One attempt per request, so a scripted single-response server is
    /// observed exactly once.
    fn single_shot(mut self) -> Self {
        self.exchange
            .set_attempt_policy(crate::http::AttemptPolicy::disabled());
        self
    }
}

impl GoogleGenerateContent {
    pub(crate) fn with_client(
        client: reqwest::Client,
        endpoint: reqwest::Url,
        endpoint_kind: EndpointKind,
        auth: GoogleAuth,
        static_headers: impl IntoIterator<Item = (String, String)>,
    ) -> Result<Self, ProviderError> {
        let (headers, redactions) = build_headers(auth, static_headers)?;
        Ok(Self {
            exchange: HttpExchange::new(
                client,
                RequestAuthorizer::default(),
                Arc::from(redactions),
            ),
            endpoint,
            endpoint_kind,
            headers: Arc::new(headers),
        })
    }

    fn request_endpoint(&self, model: &str) -> Result<reqwest::Url, ProviderError> {
        if matches!(self.endpoint_kind, EndpointKind::Exact) {
            return Ok(self.endpoint.clone());
        }

        let model = model.strip_prefix("models/").unwrap_or(model);
        if model.is_empty()
            || model.contains('/')
            || model.contains(['?', '#', '\\'])
            || model == "."
            || model == ".."
        {
            return Err(ProviderError::Configuration(
                "Google model identifier must be one non-empty URL path segment".to_owned(),
            ));
        }

        let mut endpoint = self.endpoint.clone();
        if endpoint.query().is_some() {
            return Err(ProviderError::Configuration(
                "Google base endpoint URL must not contain a query".to_owned(),
            ));
        }
        endpoint
            .path_segments_mut()
            .map_err(|()| {
                ProviderError::Configuration(
                    "Google base endpoint URL cannot contain protocol paths".to_owned(),
                )
            })?
            .pop_if_empty()
            .push("models")
            .push(&format!("{model}:streamGenerateContent"));
        endpoint.query_pairs_mut().append_pair("alt", "sse");
        Ok(endpoint)
    }
}

impl Provider for GoogleGenerateContent {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        if let Some(error) = request.unsupported_reasoning_effort("Google GenerateContent") {
            return Box::pin(async_stream::stream! { yield Err(error); });
        }
        let exchange = self.exchange.clone();
        let endpoint = self.request_endpoint(request.model()).map_err(Arc::new);
        let headers = self.headers.clone();

        with_restart(&self.exchange, move |ledger| {
            let exchange = exchange.clone();
            let endpoint = endpoint.clone();
            let headers = headers.clone();
            let request = request.clone();
            Box::pin(try_stream! {
                let endpoint = endpoint.map_err(|error| ProviderError::Configuration(error.to_string()))?;
                let max_output_tokens = i32::try_from(request.max_output_tokens()).map_err(|_| {
                    ProviderError::Configuration(
                        "Google max output tokens must not exceed 2147483647".to_owned(),
                    )
                })?;
                let limits = StreamLimits::new(request.max_output_tokens());
                let body = GenerateContentRequest::new(&request, max_output_tokens)?;
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
                    "Google GenerateContent output size overflowed",
                    "Google GenerateContent output exceeded the configured size limit",
                );
                let mut usage = UsageOnce::new(
                    "Google GenerateContent stream reported usage more than once",
                );
                let mut reasoning_open = false;
                // Gemini assigns no tool-call ids; a per-stream ordinal keeps the
                // synthesized ids deterministic.
                let mut tool_call_ordinal = 0_u64;

                while let Some(frame) = sse.next_event().await? {
                    for event in decode_event(&frame.data, &mut tool_call_ordinal, redactions.as_ref())? {
                        match event {
                            DecodedEvent::OutputText(text) => {
                                if reasoning_open {
                                    yield ProviderEvent::ReasoningCompleted {
                                        kind: crate::ReasoningKind::ExposedThinking,
                                    };
                                    reasoning_open = false;
                                }
                                output_bytes.add(text.len())?;
                                yield ProviderEvent::OutputTextDelta { text };
                            }
                            DecodedEvent::Reasoning(text) => {
                                if !reasoning_open {
                                    yield ProviderEvent::ReasoningStarted {
                                        kind: crate::ReasoningKind::ExposedThinking,
                                    };
                                    reasoning_open = true;
                                }
                                output_bytes.add(text.len())?;
                                yield ProviderEvent::ReasoningDelta {
                                    kind: crate::ReasoningKind::ExposedThinking,
                                    text,
                                };
                            }
                            DecodedEvent::Usage(event_usage) => {
                                usage.set(event_usage)?;
                            }
                            DecodedEvent::ToolCall { id, name, arguments } => {
                                if reasoning_open {
                                    yield ProviderEvent::ReasoningCompleted {
                                        kind: crate::ReasoningKind::ExposedThinking,
                                    };
                                    reasoning_open = false;
                                }
                                output_bytes.add(arguments.len())?;
                                yield ProviderEvent::ToolCallStarted {
                                    id: id.clone(),
                                    name,
                                };
                                yield ProviderEvent::ToolCallArgumentsDelta {
                                    id: id.clone(),
                                    json: arguments,
                                };
                                yield ProviderEvent::ToolCallCompleted { id };
                            }
                            DecodedEvent::Completed => {
                                if reasoning_open {
                                    yield ProviderEvent::ReasoningCompleted {
                                        kind: crate::ReasoningKind::ExposedThinking,
                                    };
                                }
                                yield ProviderEvent::Completed { usage: usage.finish() };
                                return;
                            }
                            DecodedEvent::Incomplete(reason) => {
                                if reasoning_open {
                                    yield ProviderEvent::ReasoningCompleted {
                                        kind: crate::ReasoningKind::ExposedThinking,
                                    };
                                }
                                yield ProviderEvent::Incomplete { usage: usage.finish(), reason };
                                return;
                            }
                        }
                    }
                }

                Err(sse.ended_early(
                    "Google GenerateContent stream ended before a terminal finish reason",
                ))?;
            })
        })
    }
}

fn build_headers(
    auth: GoogleAuth,
    static_headers: impl IntoIterator<Item = (String, String)>,
) -> Result<(HeaderMap, Vec<String>), ProviderError> {
    let mut redactions = Vec::new();
    let auth_header = match auth {
        GoogleAuth::NoAuth => None,
        GoogleAuth::XGoogApiKey(secret) => {
            let value = sensitive_header_value(&secret, "x-goog-api-key secret")?;
            redactions.push(secret.expose_secret().to_owned());
            Some((X_GOOG_API_KEY, value))
        }
        GoogleAuth::Bearer(secret) => {
            let value = sensitive_bearer_value(&secret, "Bearer secret")?;
            redactions.push(secret.expose_secret().to_owned());
            Some((AUTHORIZATION, value))
        }
        GoogleAuth::Header(name, secret) => {
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ProviderError::Configuration("authentication header name is invalid".to_owned())
            })?;
            if name == X_GOOG_API_KEY || is_request_controlled_header(&name) {
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
    let mut headers =
        SafeHeaders::new([AUTHORIZATION, X_GOOG_API_KEY].into_iter().chain(auth_name));
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
        "Google GenerateContent SSE event size overflowed",
        "Google GenerateContent SSE event exceeded the configured size limit",
        Utf8ErrorMessage::Static("Google GenerateContent SSE event data was not UTF-8"),
    )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GenerateContentRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<SystemInstruction<'a>>,
    contents: Vec<GoogleContent<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<GoogleTool<'a>>,
    generation_config: GenerationConfig,
}

#[derive(Serialize)]
struct SystemInstruction<'a> {
    parts: Vec<GooglePart<'a>>,
}

impl<'a> GenerateContentRequest<'a> {
    /// Builds the wire request, resolving each tool result back to the name of
    /// the call it answers. Gemini identifies function responses by name, so a
    /// result whose `call_id` matches no earlier tool call cannot be sent.
    pub(crate) fn new(
        request: &'a ModelRequest,
        max_output_tokens: i32,
    ) -> Result<Self, ProviderError> {
        let messages = request.messages();
        let mut contents = Vec::with_capacity(messages.len());
        for (index, message) in messages.iter().enumerate() {
            let mut parts = Vec::with_capacity(message.content().len());
            for block in message.content() {
                parts.push(match block {
                    ContentBlock::Text { text } => GooglePart::Text { text: Text(text) },
                    ContentBlock::ToolCall {
                        name, arguments, ..
                    } => GooglePart::FunctionCall {
                        function_call: FunctionCallPart {
                            name,
                            args: arguments,
                        },
                    },
                    ContentBlock::ToolResult {
                        call_id,
                        content,
                        is_error,
                    } => {
                        let name = messages[..index]
                            .iter()
                            .flat_map(Message::content)
                            .find_map(|earlier| match earlier {
                                ContentBlock::ToolCall { id, name, .. } if id == call_id => {
                                    Some(name.as_str())
                                }
                                _ => None,
                            })
                            .ok_or_else(|| {
                                ProviderError::Configuration(format!(
                                    "Google tool result `{call_id}` does not match any earlier tool call"
                                ))
                            })?;
                        GooglePart::FunctionResponse {
                            function_response: FunctionResponsePart {
                                name,
                                response: if *is_error {
                                    FunctionResponseBody::Error { error: Text(content) }
                                } else {
                                    FunctionResponseBody::Output { output: Text(content) }
                                },
                            },
                        }
                    }
                });
            }
            contents.push(GoogleContent {
                role: match message.role() {
                    Role::User => GoogleRole::User,
                    Role::Assistant => GoogleRole::Model,
                },
                parts,
            });
        }

        let tools = if request.tools().is_empty() {
            Vec::new()
        } else {
            vec![GoogleTool {
                function_declarations: request
                    .tools()
                    .iter()
                    .map(FunctionDeclaration::from)
                    .collect(),
            }]
        };

        Ok(Self {
            system_instruction: request.system().map(|text| SystemInstruction {
                parts: vec![GooglePart::Text { text: Text(text) }],
            }),
            contents,
            tools,
            generation_config: GenerationConfig { max_output_tokens },
        })
    }
}

#[derive(Serialize)]
struct GoogleContent<'a> {
    role: GoogleRole,
    parts: Vec<GooglePart<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum GoogleRole {
    User,
    Model,
}

#[derive(Serialize)]
#[serde(untagged)]
enum GooglePart<'a> {
    Text {
        text: Text<'a>,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: FunctionCallPart<'a>,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: FunctionResponsePart<'a>,
    },
}

#[derive(Serialize)]
struct FunctionCallPart<'a> {
    name: &'a str,
    args: &'a RawValue,
}

#[derive(Serialize)]
struct FunctionResponsePart<'a> {
    name: &'a str,
    response: FunctionResponseBody<'a>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum FunctionResponseBody<'a> {
    Output { output: Text<'a> },
    Error { error: Text<'a> },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GoogleTool<'a> {
    function_declarations: Vec<FunctionDeclaration<'a>>,
}

#[derive(Serialize)]
struct FunctionDeclaration<'a> {
    name: &'a str,
    description: &'a str,
    parameters: &'a RawValue,
}

impl<'a> From<&'a ToolSpec> for FunctionDeclaration<'a> {
    fn from(tool: &'a ToolSpec) -> Self {
        Self {
            name: tool.name(),
            description: tool.description(),
            parameters: tool.input_schema(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GenerationConfig {
    max_output_tokens: i32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerateContentResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    prompt_feedback: Option<PromptFeedback>,
    error: Option<WireApiError>,
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageMetadata {
    prompt_token_count: u64,
    candidates_token_count: u64,
    #[serde(default)]
    cached_content_token_count: u64,
    #[serde(default)]
    thoughts_token_count: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Candidate {
    content: Option<ResponseContent>,
    finish_reason: Option<String>,
    finish_message: Option<String>,
    index: Option<u32>,
}

#[derive(Deserialize)]
struct ResponseContent {
    #[serde(default)]
    parts: Vec<ResponsePart>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponsePart {
    text: Option<String>,
    #[serde(default)]
    thought: bool,
    function_call: Option<WireFunctionCall>,
    executable_code: Option<Value>,
    code_execution_result: Option<Value>,
    inline_data: Option<Value>,
    file_data: Option<Value>,
    thought_signature: Option<String>,
    #[serde(flatten)]
    unknown: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct WireFunctionCall {
    name: String,
    args: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PromptFeedback {
    block_reason: Option<String>,
}

#[derive(Deserialize)]
struct ApiErrorEnvelope {
    error: WireApiError,
}

#[derive(Deserialize)]
struct WireApiError {
    code: Option<u16>,
    message: Option<String>,
    status: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum DecodedEvent {
    OutputText(String),
    Reasoning(String),
    Usage(ProviderUsage),
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    Completed,
    Incomplete(IncompleteReason),
}

fn decode_event(
    data: &str,
    tool_call_ordinal: &mut u64,
    redactions: &[String],
) -> Result<Vec<DecodedEvent>, ProviderError> {
    let response: GenerateContentResponse = serde_json::from_str(data).map_err(|error| {
        ProviderError::Protocol(sanitize_message(
            &format!("could not decode Google GenerateContent event: {error}"),
            redactions,
        ))
    })?;
    if let Some(error) = response.error {
        return Err(wire_api_error(error, redactions));
    }
    if let Some(reason) = response
        .prompt_feedback
        .and_then(|feedback| feedback.block_reason)
    {
        return Err(ProviderError::ResponseFailed {
            kind: ProviderErrorKind::Response,
            message: sanitize_message(
                &format!("Google blocked the prompt with reason {reason}"),
                redactions,
            ),
        });
    }

    let usage = response.usage_metadata.map(provider_usage).transpose()?;

    let Some(candidate) = response.candidates.into_iter().next() else {
        return Ok(usage.map_or_else(Vec::new, |usage| vec![DecodedEvent::Usage(usage)]));
    };
    if candidate.index.is_some_and(|index| index != 0) {
        return Err(ProviderError::Protocol(
            "Google GenerateContent response did not contain candidate zero".to_owned(),
        ));
    }

    let mut events = usage.map_or_else(Vec::new, |usage| vec![DecodedEvent::Usage(usage)]);
    if let Some(content) = candidate.content {
        for part in content.parts {
            if part.executable_code.is_some()
                || part.code_execution_result.is_some()
                || part.inline_data.is_some()
                || part.file_data.is_some()
                || !part.unknown.is_empty()
            {
                return Err(ProviderError::Protocol(
                    "Google GenerateContent response contained unsupported non-text content"
                        .to_owned(),
                ));
            }
            if let Some(call) = part.function_call {
                if part.text.is_some() {
                    return Err(ProviderError::Protocol(
                        "Google GenerateContent response mixed text and a function call in one part"
                            .to_owned(),
                    ));
                }
                let arguments = call.args.unwrap_or_else(|| Value::Object(Map::new()));
                let arguments = serde_json::to_string(&arguments).map_err(|error| {
                    ProviderError::Protocol(sanitize_message(
                        &format!("could not serialize Google function-call arguments: {error}"),
                        redactions,
                    ))
                })?;
                // Gemini assigns no call ids; synthesize a deterministic one
                // from the per-stream ordinal and the function name.
                let id = format!("call_{tool_call_ordinal}_{name}", name = call.name);
                *tool_call_ordinal += 1;
                events.push(DecodedEvent::ToolCall {
                    id,
                    name: call.name,
                    arguments,
                });
                continue;
            }
            if part.thought {
                if part.thought_signature.is_some() {
                    return Err(ProviderError::Protocol(
                        "Google GenerateContent response mixed displayable thought text with opaque continuation state"
                            .to_owned(),
                    ));
                }
                if let Some(text) = part.text.filter(|text| !text.is_empty()) {
                    events.push(DecodedEvent::Reasoning(text));
                }
                continue;
            }
            if part.thought_signature.is_some() {
                return Err(ProviderError::Protocol(
                    "Google GenerateContent response contained a thought signature without thought content"
                        .to_owned(),
                ));
            }
            if let Some(text) = part.text.filter(|text| !text.is_empty()) {
                events.push(DecodedEvent::OutputText(text));
            }
        }
    }

    if let Some(reason) = candidate.finish_reason {
        match reason.as_str() {
            "STOP" | "TOOL_CALL" | "TOOL_CALLS" => events.push(DecodedEvent::Completed),
            "MAX_TOKENS" => events.push(DecodedEvent::Incomplete(IncompleteReason::OutputTokens)),
            "MALFORMED_FUNCTION_CALL"
            | "UNEXPECTED_TOOL_CALL"
            | "TOO_MANY_TOOL_CALLS"
            | "MISSING_THOUGHT_SIGNATURE"
            | "MALFORMED_RESPONSE" => {
                return Err(ProviderError::Protocol(format!(
                    "Google response ended with tool or protocol failure reason {reason}"
                )));
            }
            "FINISH_REASON_UNSPECIFIED" => {
                return Err(ProviderError::Protocol(
                    "Google response used an unspecified finish reason".to_owned(),
                ));
            }
            _ => {
                let detail = candidate
                    .finish_message
                    .filter(|message| !message.trim().is_empty())
                    .map_or_else(
                        || format!("Google response was blocked with reason {reason}"),
                        |message| {
                            format!("Google response was blocked with reason {reason}: {message}")
                        },
                    );
                return Err(ProviderError::ResponseFailed {
                    kind: ProviderErrorKind::Response,
                    message: sanitize_message(&detail, redactions),
                });
            }
        }
    }
    Ok(events)
}

fn provider_usage(usage: UsageMetadata) -> Result<ProviderUsage, ProviderError> {
    let input_tokens = subtract_cached_input_tokens(
        usage.prompt_token_count,
        usage.cached_content_token_count,
        "Google cached input tokens exceeded prompt tokens",
    )?;
    let output_tokens = usage
        .candidates_token_count
        .checked_add(usage.thoughts_token_count)
        .ok_or_else(|| {
            ProviderError::Protocol("Google output token usage overflowed".to_owned())
        })?;
    Ok(ProviderUsage {
        input_tokens,
        cache_read_input_tokens: usage.cached_content_token_count,
        cache_write_input_tokens: 0,
        output_tokens,
        reasoning_tokens: Some(usage.thoughts_token_count),
    })
}

fn wire_api_error(error: WireApiError, redactions: &[String]) -> ProviderError {
    let kind = error
        .code
        .map(status_error_kind)
        .or_else(|| error.status.as_deref().map(named_error_kind))
        .unwrap_or(ProviderErrorKind::Response);
    let message = error.message.as_deref().map_or_else(
        || "Google did not provide an error message".to_owned(),
        |message| sanitize_message(message, redactions),
    );
    ProviderError::ResponseFailed { kind, message }
}

fn named_error_kind(name: &str) -> ProviderErrorKind {
    match name {
        "UNAUTHENTICATED" | "PERMISSION_DENIED" => ProviderErrorKind::Authentication,
        "RESOURCE_EXHAUSTED" => ProviderErrorKind::RateLimited,
        "INVALID_ARGUMENT" | "FAILED_PRECONDITION" | "NOT_FOUND" | "OUT_OF_RANGE" => {
            ProviderErrorKind::InvalidRequest
        }
        "INTERNAL" | "UNAVAILABLE" | "DEADLINE_EXCEEDED" => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Response,
    }
}

fn api_error(rejection: HttpRejection) -> ProviderError {
    support::api_error(
        rejection,
        "Google GenerateContent request failed",
        |envelope: ApiErrorEnvelope| envelope.error.message,
    )
}

#[cfg(test)]
mod tests {
    use futures_util::StreamExt;
    use serde_json::json;

    use crate::test_support::LoopbackServer;

    use super::*;

    #[tokio::test]
    async fn streams_text_and_builds_the_google_wire_request() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hel\"}]},\"index\":0}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"lo\"}]},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":20,\"cachedContentTokenCount\":6,\"candidatesTokenCount\":7,\"thoughtsTokenCount\":3}}\n\n",
        );
        let server = LoopbackServer::sse(body);
        let base_url = server.base_url.clone();
        let provider = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint(&base_url, true).unwrap(),
            EndpointKind::Base,
            GoogleAuth::XGoogApiKey("google-test-secret".into()),
            [],
        )
        .map(GoogleGenerateContent::single_shot)
        .unwrap();

        let events = provider
            .stream(ModelRequest::new(
                "models/gemini-test",
                vec![Message::user("hello"), Message::assistant("hi")],
                64,
            ))
            .collect::<Vec<_>>()
            .await;

        assert!(matches!(
            &events[..],
            [
                Ok(ProviderEvent::OutputTextDelta { text: first }),
                Ok(ProviderEvent::OutputTextDelta { text: second }),
                Ok(ProviderEvent::Completed {
                    usage: Some(ProviderUsage {
                        input_tokens: 14,
                        cache_read_input_tokens: 6,
                        cache_write_input_tokens: 0,
                        output_tokens: 10,
                        ..
                    }),
                }),
            ] if first == "Hel" && second == "lo"
        ));
        let request = server.capture();
        assert_eq!(
            request.request_line(),
            Some("POST /models/gemini-test:streamGenerateContent?alt=sse HTTP/1.1")
        );
        assert_eq!(request.header("x-goog-api-key"), Some("google-test-secret"));
        assert_eq!(
            request.json_body(),
            serde_json::json!({
                "contents": [
                    {"role": "user", "parts": [{"text": "hello"}]},
                    {"role": "model", "parts": [{"text": "hi"}]},
                ],
                "generationConfig": {"maxOutputTokens": 64},
            })
        );
    }

    #[test]
    fn maps_the_system_prompt_to_the_system_instruction_field() {
        let request = ModelRequest::new("gemini-test", vec![Message::user("ping")], 64)
            .with_system("You are QQ.");
        let body =
            serde_json::to_value(GenerateContentRequest::new(&request, 64).unwrap()).unwrap();
        assert_eq!(
            body["systemInstruction"],
            json!({"parts": [{"text": "You are QQ."}]})
        );
        assert_eq!(body["contents"][0]["parts"][0]["text"], "ping");

        let without = ModelRequest::new("gemini-test", vec![Message::user("ping")], 64);
        let body =
            serde_json::to_value(GenerateContentRequest::new(&without, 64).unwrap()).unwrap();
        assert!(body.get("systemInstruction").is_none());
    }

    #[tokio::test]
    async fn sends_tool_declarations_and_tool_history_parts() {
        let body = "data: {\"candidates\":[{\"finishReason\":\"STOP\",\"index\":0}]}\n\n";
        let server = LoopbackServer::sse(body);
        let endpoint = server.base_url.clone();
        let provider = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint(&endpoint, true).unwrap(),
            EndpointKind::Base,
            GoogleAuth::NoAuth,
            [],
        )
        .map(GoogleGenerateContent::single_shot)
        .unwrap();
        let request = ModelRequest::new(
            "gemini-test",
            vec![
                Message::user("read the config"),
                Message::new(
                    Role::Assistant,
                    vec![
                        ContentBlock::Text {
                            text: "Reading it now.".to_owned(),
                        },
                        ContentBlock::tool_call(
                            "call_0_read_file".to_owned(),
                            "read_file".to_owned(),
                            &serde_json::json!({"path": "config.ron"}),
                        ),
                        ContentBlock::tool_call(
                            "call_1_list_dir".to_owned(),
                            "list_dir".to_owned(),
                            &serde_json::json!({"path": "."}),
                        ),
                    ],
                ),
                Message::tool_results(vec![
                    ContentBlock::ToolResult {
                        call_id: "call_0_read_file".to_owned(),
                        content: "(config)".to_owned(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        call_id: "call_1_list_dir".to_owned(),
                        content: "denied".to_owned(),
                        is_error: true,
                    },
                ]),
            ],
            128,
        )
        .with_tools(vec![ToolSpec::new(
            "read_file",
            "Reads one file",
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )]);
        let events = provider.stream(request).collect::<Vec<_>>().await;

        assert!(matches!(
            &events[..],
            [Ok(ProviderEvent::Completed { usage: None })]
        ));
        assert_eq!(
            server.capture().json_body(),
            serde_json::json!({
                "contents": [
                    {"role": "user", "parts": [{"text": "read the config"}]},
                    {"role": "model", "parts": [
                        {"text": "Reading it now."},
                        {"functionCall": {"name": "read_file", "args": {"path": "config.ron"}}},
                        {"functionCall": {"name": "list_dir", "args": {"path": "."}}},
                    ]},
                    {"role": "user", "parts": [
                        {"functionResponse": {
                            "name": "read_file",
                            "response": {"output": "(config)"},
                        }},
                        {"functionResponse": {
                            "name": "list_dir",
                            "response": {"error": "denied"},
                        }},
                    ]},
                ],
                "tools": [{"functionDeclarations": [{
                    "name": "read_file",
                    "description": "Reads one file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}},
                }]}],
                "generationConfig": {"maxOutputTokens": 128},
            })
        );
    }

    #[tokio::test]
    async fn streams_thoughts_as_a_bounded_reasoning_block() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"checking\"}]},\"index\":0}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\" constraints\"},{\"text\":\"answer\"}]},\"finishReason\":\"STOP\",\"index\":0}]}\n\n",
        );
        let server = LoopbackServer::sse(body);
        let endpoint = server.base_url.clone();
        let provider = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint(&endpoint, true).unwrap(),
            EndpointKind::Exact,
            GoogleAuth::NoAuth,
            [],
        )
        .map(GoogleGenerateContent::single_shot)
        .unwrap();

        let events = provider
            .stream(ModelRequest::new(
                "gemini-test",
                vec![Message::user("hi")],
                64,
            ))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        server.capture();

        assert_eq!(
            events,
            vec![
                ProviderEvent::ReasoningStarted {
                    kind: crate::ReasoningKind::ExposedThinking,
                },
                ProviderEvent::ReasoningDelta {
                    kind: crate::ReasoningKind::ExposedThinking,
                    text: "checking".to_owned(),
                },
                ProviderEvent::ReasoningDelta {
                    kind: crate::ReasoningKind::ExposedThinking,
                    text: " constraints".to_owned(),
                },
                ProviderEvent::ReasoningCompleted {
                    kind: crate::ReasoningKind::ExposedThinking,
                },
                ProviderEvent::OutputTextDelta {
                    text: "answer".to_owned(),
                },
                ProviderEvent::Completed { usage: None },
            ]
        );
    }

    #[tokio::test]
    async fn streams_tool_calls_with_deterministic_synthetic_ids_to_completion() {
        let body = concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Checking.\"}]},\"index\":0}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[",
            "{\"functionCall\":{\"name\":\"read_file\",\"args\":{\"path\":\"a.rs\"}}},",
            "{\"functionCall\":{\"name\":\"read_file\",\"args\":{\"path\":\"b.rs\"}}}",
            "]},\"finishReason\":\"STOP\",\"index\":0}]}\n\n",
        );
        let server = LoopbackServer::sse(body);
        let endpoint = server.base_url.clone();
        let provider = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint(&endpoint, true).unwrap(),
            EndpointKind::Exact,
            GoogleAuth::NoAuth,
            [],
        )
        .map(GoogleGenerateContent::single_shot)
        .unwrap();
        let events = provider
            .stream(ModelRequest::new(
                "gemini-test",
                vec![Message::user("hi")],
                64,
            ))
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        server.capture();

        assert_eq!(
            events,
            vec![
                ProviderEvent::OutputTextDelta {
                    text: "Checking.".to_owned(),
                },
                ProviderEvent::ToolCallStarted {
                    id: "call_0_read_file".to_owned(),
                    name: "read_file".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_0_read_file".to_owned(),
                    json: "{\"path\":\"a.rs\"}".to_owned(),
                },
                ProviderEvent::ToolCallCompleted {
                    id: "call_0_read_file".to_owned(),
                },
                ProviderEvent::ToolCallStarted {
                    id: "call_1_read_file".to_owned(),
                    name: "read_file".to_owned(),
                },
                ProviderEvent::ToolCallArgumentsDelta {
                    id: "call_1_read_file".to_owned(),
                    json: "{\"path\":\"b.rs\"}".to_owned(),
                },
                ProviderEvent::ToolCallCompleted {
                    id: "call_1_read_file".to_owned(),
                },
                ProviderEvent::Completed { usage: None },
            ]
        );
    }

    #[tokio::test]
    async fn rejects_a_tool_result_without_a_matching_call() {
        let provider = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint("https://example.test/custom", false).unwrap(),
            EndpointKind::Exact,
            GoogleAuth::NoAuth,
            [],
        )
        .map(GoogleGenerateContent::single_shot)
        .unwrap();
        let request = ModelRequest::new(
            "gemini-test",
            vec![Message::tool_results(vec![ContentBlock::ToolResult {
                call_id: "call_9_missing".to_owned(),
                content: "(orphaned)".to_owned(),
                is_error: false,
            }])],
            64,
        );

        let error = provider.stream(request).next().await.unwrap().unwrap_err();
        assert!(matches!(error, ProviderError::Configuration(_)));
    }

    #[test]
    fn decoder_handles_fragmented_utf8_multiline_data_and_crlf() {
        let mut decoder = sse_decoder(4_096);
        let payload = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hé\"}]},\r\n";
        let suffix = "data: \"finishReason\":\"STOP\",\"index\":0}]}\r\n\r\n";
        let mut bytes = [payload.as_bytes(), suffix.as_bytes()].concat();
        let split = bytes.iter().position(|byte| *byte == 0xc3).unwrap() + 1;
        let remainder = bytes.split_off(split);

        assert!(decoder.push(&bytes).unwrap().is_empty());
        let events = decoder.push(&remainder).unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].data.contains("hé"));
    }

    #[test]
    fn maps_terminal_and_blocked_responses_without_silent_success() {
        let max_tokens = decode_event(
            r#"{"candidates":[{"finishReason":"MAX_TOKENS","index":0}]}"#,
            &mut 0,
            &[],
        )
        .unwrap();
        assert!(matches!(
            max_tokens.as_slice(),
            [DecodedEvent::Incomplete(IncompleteReason::OutputTokens)]
        ));

        let blocked = decode_event(
            r#"{"promptFeedback":{"blockReason":"SAFETY"}}"#,
            &mut 0,
            &[],
        )
        .unwrap_err();
        assert!(matches!(
            blocked,
            ProviderError::ResponseFailed {
                kind: ProviderErrorKind::Response,
                ..
            }
        ));

        let tool = decode_event(
            r#"{"candidates":[{"finishReason":"UNEXPECTED_TOOL_CALL","index":0}]}"#,
            &mut 0,
            &[],
        )
        .unwrap_err();
        assert!(matches!(tool, ProviderError::Protocol(_)));
    }

    #[test]
    fn usage_subtracts_cached_prompt_and_includes_thoughts() {
        let events = decode_event(
            r#"{"candidates":[{"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":20,"cachedContentTokenCount":6,"candidatesTokenCount":7,"thoughtsTokenCount":3}}"#,
            &mut 0,
            &[],
        )
        .unwrap();
        assert_eq!(
            events,
            [
                DecodedEvent::Usage(ProviderUsage {
                    input_tokens: 14,
                    cache_read_input_tokens: 6,
                    cache_write_input_tokens: 0,
                    output_tokens: 10,
                    reasoning_tokens: Some(3),
                }),
                DecodedEvent::Completed,
            ]
        );

        for data in [
            r#"{"usageMetadata":{"promptTokenCount":2,"cachedContentTokenCount":3,"candidatesTokenCount":1}}"#,
            r#"{"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":18446744073709551615,"thoughtsTokenCount":1}}"#,
        ] {
            assert!(matches!(
                decode_event(data, &mut 0, &[]),
                Err(ProviderError::Protocol(_))
            ));
        }
    }

    #[test]
    fn streamed_api_errors_are_typed_and_redacted() {
        let secret = "streamed-google-secret";
        let error = decode_event(
            &format!(
                r#"{{"error":{{"code":429,"status":"RESOURCE_EXHAUSTED","message":"quota for {secret}"}}}}"#
            ),
            &mut 0,
            &[secret.to_owned()],
        )
        .unwrap_err();

        assert_eq!(error.kind(), ProviderErrorKind::RateLimited);
        assert!(!error.to_string().contains(secret));
        assert!(error.to_string().contains("[REDACTED]"));
    }

    #[test]
    fn rejects_unknown_and_mixed_non_text_parts() {
        for response in [
            r#"{"candidates":[{"content":{"parts":[{"functionResponse":{}}]},"finishReason":"STOP","index":0}]}"#,
            r#"{"candidates":[{"content":{"parts":[{"text":"unsafe","functionCall":{"name":"noop"}}]},"finishReason":"STOP","index":0}]}"#,
        ] {
            let error = decode_event(response, &mut 0, &[]).unwrap_err();
            assert!(matches!(error, ProviderError::Protocol(_)));
        }
    }

    #[test]
    fn rejects_model_path_injection_and_auth_header_overrides() {
        let endpoint = validate_endpoint("https://example.test/v1beta", false).unwrap();
        let provider = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            endpoint.clone(),
            EndpointKind::Base,
            GoogleAuth::NoAuth,
            [],
        )
        .map(GoogleGenerateContent::single_shot)
        .unwrap();
        assert!(provider.request_endpoint("../secret").is_err());
        assert!(provider.request_endpoint("model?key=secret").is_err());

        let exact = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint("https://example.test/custom?alt=sse", false).unwrap(),
            EndpointKind::Exact,
            GoogleAuth::NoAuth,
            [],
        )
        .map(GoogleGenerateContent::single_shot)
        .unwrap();
        assert_eq!(
            exact.request_endpoint("../not-used").unwrap().as_str(),
            "https://example.test/custom?alt=sse"
        );

        assert!(
            GoogleGenerateContent::with_client(
                crate::http::build_direct_client().unwrap(),
                endpoint,
                EndpointKind::Base,
                GoogleAuth::XGoogApiKey("secret".into()),
                [("x-goog-api-key".to_owned(), "override".to_owned())],
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn redacts_api_keys_from_http_errors() {
        let secret = "google-secret-value";
        let server = LoopbackServer::respond(
            403,
            "application/json",
            format!(r#"{{"error":{{"message":"bad key {secret}"}}}}"#),
        );
        let endpoint = server.base_url.clone();
        let provider = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint(&endpoint, true).unwrap(),
            EndpointKind::Exact,
            GoogleAuth::XGoogApiKey(secret.into()),
            [],
        )
        .map(GoogleGenerateContent::single_shot)
        .unwrap();

        let error = provider
            .stream(ModelRequest::new(
                "gemini-test",
                vec![Message::user("hi")],
                64,
            ))
            .next()
            .await
            .unwrap()
            .unwrap_err();
        server.capture();

        assert!(!error.to_string().contains(secret));
        assert!(error.to_string().contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn rejects_incomplete_and_non_sse_responses() {
        let incomplete_server = LoopbackServer::sse(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial\"}]},\"index\":0}]}\n\n",
        );
        let endpoint = incomplete_server.base_url.clone();
        let incomplete = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint(&endpoint, true).unwrap(),
            EndpointKind::Exact,
            GoogleAuth::NoAuth,
            [],
        )
        .map(GoogleGenerateContent::single_shot)
        .unwrap()
        .stream(ModelRequest::new(
            "gemini-test",
            vec![Message::user("hi")],
            64,
        ))
        .collect::<Vec<_>>()
        .await;
        incomplete_server.capture();
        assert!(matches!(
            &incomplete[..],
            [
                Ok(ProviderEvent::OutputTextDelta { text }),
                Err(ProviderError::Protocol(_)),
            ] if text == "partial"
        ));

        let non_sse_server = LoopbackServer::respond(200, "application/json", "{}");
        let endpoint = non_sse_server.base_url.clone();
        let error = GoogleGenerateContent::with_client(
            crate::http::build_direct_client().unwrap(),
            validate_endpoint(&endpoint, true).unwrap(),
            EndpointKind::Exact,
            GoogleAuth::NoAuth,
            [],
        )
        .map(GoogleGenerateContent::single_shot)
        .unwrap()
        .stream(ModelRequest::new(
            "gemini-test",
            vec![Message::user("hi")],
            64,
        ))
        .next()
        .await
        .unwrap()
        .unwrap_err();
        non_sse_server.capture();
        assert!(matches!(error, ProviderError::Protocol(_)));
    }

    #[test]
    fn enforces_event_output_and_wire_limits() {
        let mut decoder = sse_decoder(4);
        assert!(matches!(
            decoder.push(b"data: value"),
            Err(ProviderError::Protocol(_))
        ));

        let mut output = ByteCounter::new(4, "output overflow", "output limit");
        assert!(matches!(output.add(5), Err(ProviderError::Protocol(_))));
        let mut wire = ByteCounter::new(4, "wire overflow", "wire limit");
        assert!(matches!(wire.add(5), Err(ProviderError::Protocol(_))));
    }
}
