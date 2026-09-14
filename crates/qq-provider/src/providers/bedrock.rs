//! Amazon Bedrock `ConverseStream` adapter.

use std::{
    collections::{HashMap, hash_map::Entry},
    error::Error,
    fmt::{self, Write as _},
    pin::Pin,
    sync::Arc,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll},
};

use async_stream::try_stream;
use aws_config::{SdkConfig, retry::RetryConfig};
use aws_sdk_bedrockruntime::{
    Client,
    config::{
        Config, ConfigBag, Intercept, RuntimeComponents, Token,
        interceptors::BeforeDeserializationInterceptorContextMut,
    },
    error::{BoxError, DisplayErrorContext, SdkError},
    operation::converse_stream::ConverseStreamError,
    types::{
        ContentBlock as BedrockContentBlock, ContentBlockDelta, ContentBlockStart,
        ConversationRole, ConverseStreamOutput, InferenceConfiguration, Message as BedrockMessage,
        StopReason, SystemContentBlock, TokenUsage, Tool, ToolConfiguration, ToolInputSchema,
        ToolResultBlock, ToolResultContentBlock, ToolResultStatus, ToolSpecification, ToolUseBlock,
        error::ConverseStreamOutputError,
    },
};
use aws_smithy_types::{Document, Number, body::SdkBody};
use bytes::Bytes;
use http_body::Body;
use serde_json::Value;
use tokio::sync::OnceCell;

use crate::{
    ContentBlock, IncompleteReason, ModelRequest, Provider, ProviderError, ProviderErrorKind,
    ProviderEvent, ProviderStream, ProviderUsage, Role,
    aws::{AwsConfigLoadError, load_aws_config, validate_configuration},
    bedrock_auth::BedrockAuth,
    limits::{ByteCounter, StreamLimits},
    sanitize::sanitize_message,
};

const EVENT_FRAME_OVERHEAD_BYTES: usize = 64;

/// A client for Amazon Bedrock's `ConverseStream` API.
#[derive(Clone)]
pub(crate) struct Bedrock {
    client: Arc<OnceCell<Client>>,
    auth: BedrockAuth,
    region: Option<String>,
    redactions: Arc<[String]>,
}

impl fmt::Debug for Bedrock {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Bedrock").finish_non_exhaustive()
    }
}

impl Bedrock {
    /// Creates a lazily initialized Bedrock client.
    ///
    /// AWS configuration is not loaded and no network access occurs until the
    /// first returned provider stream is polled. If `region` is `None`, the AWS
    /// region provider chain is used.
    ///
    /// # Errors
    ///
    /// Returns an error if the authentication or region configuration is empty or contains
    /// control characters.
    pub(crate) fn new(auth: BedrockAuth, region: Option<String>) -> Result<Self, ProviderError> {
        validate_configuration(&auth, region.as_deref())?;
        let redactions: Arc<[String]> = match &auth {
            BedrockAuth::ApiKey(secret) => Arc::from([secret.expose_secret().to_owned()]),
            BedrockAuth::DefaultChain | BedrockAuth::Profile(_) => Arc::from([]),
        };
        Ok(Self {
            client: Arc::new(OnceCell::new()),
            auth,
            region,
            redactions,
        })
    }
}

impl Provider for Bedrock {
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        let client = self.client.clone();
        let auth = self.auth.clone();
        let region = self.region.clone();
        let redactions = Arc::clone(&self.redactions);

        Box::pin(try_stream! {
            let limits = StreamLimits::new(request.max_output_tokens());
            let request = ConverseRequest::try_from(&request)?;
            let client = match client.get_or_try_init(|| load_client(auth, region)).await {
                Ok(client) => client,
                Err(error) => Err(error.to_provider_error())?,
            };
            let body_limit = ResponseBodyLimit::new(limits.wire);
            let body_limit_exceeded = Arc::clone(&body_limit.exceeded);
            let response = client
                .converse_stream()
                .model_id(request.model_id)
                .set_system(request.system)
                .set_messages(Some(request.messages))
                .set_tool_config(request.tool_config)
                .inference_config(request.inference_config)
                .customize()
                .interceptor(body_limit)
                .send()
                .await
                .map_err(|error| {
                    request_error(
                        &error,
                        redactions.as_ref(),
                        body_limit_exceeded.load(Ordering::Relaxed),
                    )
                })?;
            let mut receiver = response.stream;
            let mut output_bytes = ByteCounter::new(
                limits.output,
                "Amazon Bedrock output size overflowed",
                "Amazon Bedrock output exceeded the configured size limit",
            );
            let mut message_stopped = false;
            let mut incomplete = None;
            // Maps streamed content-block indexes to tool-call ids so argument
            // deltas and block stops can be attributed after the start event.
            let mut tool_calls = ToolCallTracker::default();

            while let Some(event) = receiver
                .recv()
                .await
                .map_err(|error| {
                    stream_error(
                        &error,
                        redactions.as_ref(),
                        body_limit_exceeded.load(Ordering::Relaxed),
                    )
                })?
            {
                check_stream_event_size(&event, limits.event)?;

                match decode_stream_event(event)? {
                    DecodedEvent::OutputText(text) => {
                        if message_stopped {
                            Err(ProviderError::Protocol(
                                "Amazon Bedrock returned output after messageStop".to_owned(),
                            ))?;
                        }
                        if text.is_empty() {
                            continue;
                        }
                        output_bytes.add(text.len())?;
                        yield ProviderEvent::OutputTextDelta { text };
                    }
                    DecodedEvent::Refusal(text) => {
                        if message_stopped {
                            Err(ProviderError::Protocol(
                                "Amazon Bedrock returned more than one messageStop".to_owned(),
                            ))?;
                        }
                        output_bytes.add(text.len())?;
                        yield ProviderEvent::RefusalDelta { text };
                        message_stopped = true;
                    }
                    DecodedEvent::MessageStopped => {
                        if message_stopped {
                            Err(ProviderError::Protocol(
                                "Amazon Bedrock returned more than one messageStop".to_owned(),
                            ))?;
                        }
                        message_stopped = true;
                    }
                    DecodedEvent::MessageIncomplete(reason) => {
                        if message_stopped {
                            Err(ProviderError::Protocol(
                                "Amazon Bedrock returned more than one messageStop".to_owned(),
                            ))?;
                        }
                        message_stopped = true;
                        incomplete = Some(reason);
                    }
                    DecodedEvent::ToolCallStarted { index, id, name } => {
                        if message_stopped {
                            Err(ProviderError::Protocol(
                                "Amazon Bedrock returned output after messageStop".to_owned(),
                            ))?;
                        }
                        tool_calls.start(index, id.clone())?;
                        yield ProviderEvent::ToolCallStarted { id, name };
                    }
                    DecodedEvent::ToolCallArguments { index, json } => {
                        if message_stopped {
                            Err(ProviderError::Protocol(
                                "Amazon Bedrock returned output after messageStop".to_owned(),
                            ))?;
                        }
                        let id = tool_calls.arguments(index)?.to_owned();
                        output_bytes.add(json.len())?;
                        yield ProviderEvent::ToolCallArgumentsDelta { id, json };
                    }
                    DecodedEvent::BlockStopped { index } => {
                        if message_stopped {
                            Err(ProviderError::Protocol(
                                "Amazon Bedrock returned output after messageStop".to_owned(),
                            ))?;
                        }
                        if let Some(id) = tool_calls.stop(index) {
                            yield ProviderEvent::ToolCallCompleted { id };
                        }
                    }
                    DecodedEvent::Usage(usage) => {
                        if !message_stopped {
                            Err(ProviderError::Protocol(
                                "Amazon Bedrock returned metadata before messageStop".to_owned(),
                            ))?;
                        }
                        match incomplete {
                            Some(reason) => {
                                yield ProviderEvent::Incomplete { usage: Some(usage), reason };
                            }
                            None => yield ProviderEvent::Completed { usage: Some(usage) },
                        }
                        return;
                    }
                    DecodedEvent::Ignored => {}
                }
            }

            let message = if message_stopped {
                "Amazon Bedrock stream ended before metadata"
            } else {
                "Amazon Bedrock stream ended before messageStop"
            };
            Err(ProviderError::Protocol(message.to_owned()))?;
        })
    }
}

async fn load_client(
    auth: BedrockAuth,
    region: Option<String>,
) -> Result<Client, AwsConfigLoadError> {
    let loaded = match load_aws_config(&auth, region.as_deref()).await {
        Ok(config) => config,
        Err(error) => return Err(error),
    };
    let api_key = match auth {
        BedrockAuth::ApiKey(secret) => Some(secret.expose_secret().to_owned()),
        BedrockAuth::DefaultChain | BedrockAuth::Profile(_) => None,
    };
    Ok(Client::from_conf(service_config(
        &loaded.sdk_config,
        api_key,
    )))
}

fn service_config(shared_config: &SdkConfig, api_key: Option<String>) -> Config {
    let builder = aws_sdk_bedrockruntime::config::Builder::from(shared_config)
        .retry_config(RetryConfig::disabled());
    let builder = if let Some(api_key) = api_key {
        builder
            .bearer_token(Token::new(api_key, None))
            .auth_scheme_preference(["httpBearerAuth".into()])
    } else {
        builder.auth_scheme_preference(["sigv4".into()])
    };

    builder.build()
}

#[derive(Debug)]
struct ConverseRequest {
    model_id: String,
    system: Option<Vec<SystemContentBlock>>,
    messages: Vec<BedrockMessage>,
    tool_config: Option<ToolConfiguration>,
    inference_config: InferenceConfiguration,
}

impl TryFrom<&ModelRequest> for ConverseRequest {
    type Error = ProviderError;

    fn try_from(request: &ModelRequest) -> Result<Self, Self::Error> {
        let max_tokens = i32::try_from(request.max_output_tokens()).map_err(|_| {
            ProviderError::Configuration(
                "Amazon Bedrock max_output_tokens must not exceed 2147483647".to_owned(),
            )
        })?;
        let messages = request
            .messages()
            .iter()
            .map(|message| {
                let mut builder = BedrockMessage::builder().role(match message.role() {
                    Role::User => ConversationRole::User,
                    Role::Assistant => ConversationRole::Assistant,
                });
                for block in message.content() {
                    builder = builder.content(bedrock_content_block(block)?);
                }
                builder.build().map_err(|_| {
                    ProviderError::Configuration(
                        "could not construct an Amazon Bedrock message".to_owned(),
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let tool_config = if request.tools().is_empty() {
            None
        } else {
            let mut builder = ToolConfiguration::builder();
            for tool in request.tools() {
                let specification = ToolSpecification::builder()
                    .name(tool.name())
                    .description(tool.description())
                    .input_schema(ToolInputSchema::Json(document_from_raw(
                        tool.input_schema(),
                    )?))
                    .build()
                    .map_err(|_| {
                        ProviderError::Configuration(
                            "could not construct an Amazon Bedrock tool specification".to_owned(),
                        )
                    })?;
                builder = builder.tools(Tool::ToolSpec(specification));
            }
            Some(builder.build().map_err(|_| {
                ProviderError::Configuration(
                    "could not construct an Amazon Bedrock tool configuration".to_owned(),
                )
            })?)
        };

        Ok(Self {
            model_id: request.model().to_owned(),
            system: request
                .system()
                .map(|system| vec![SystemContentBlock::Text(system.to_owned())]),
            messages,
            tool_config,
            inference_config: InferenceConfiguration::builder()
                .max_tokens(max_tokens)
                .build(),
        })
    }
}

fn bedrock_content_block(block: &ContentBlock) -> Result<BedrockContentBlock, ProviderError> {
    match block {
        ContentBlock::Text { text } => Ok(BedrockContentBlock::Text(text.clone())),
        ContentBlock::ToolCall {
            id,
            name,
            arguments,
        } => ToolUseBlock::builder()
            .tool_use_id(id)
            .name(name)
            .input(document_from_raw(arguments)?)
            .build()
            .map(BedrockContentBlock::ToolUse)
            .map_err(|_| {
                ProviderError::Configuration(
                    "could not construct an Amazon Bedrock tool use block".to_owned(),
                )
            }),
        ContentBlock::ToolResult {
            call_id,
            content,
            is_error,
        } => {
            let mut builder = ToolResultBlock::builder()
                .tool_use_id(call_id)
                .content(ToolResultContentBlock::Text(content.clone()));
            if *is_error {
                builder = builder.status(ToolResultStatus::Error);
            }
            builder
                .build()
                .map(BedrockContentBlock::ToolResult)
                .map_err(|_| {
                    ProviderError::Configuration(
                        "could not construct an Amazon Bedrock tool result block".to_owned(),
                    )
                })
        }
    }
}

/// The Converse SDK wants a `Document` tree, so this adapter alone parses
/// the compact JSON the request carries. The text was produced by
/// `serde_json`, so a parse failure is a programming error, reported rather
/// than trusted.
fn document_from_raw(raw: &serde_json::value::RawValue) -> Result<Document, ProviderError> {
    serde_json::from_str::<Value>(raw.get())
        .map(|value| document_from_value(&value))
        .map_err(|error| {
            ProviderError::Configuration(format!(
                "could not parse JSON for an Amazon Bedrock document: {error}"
            ))
        })
}

fn document_from_value(value: &Value) -> Document {
    match value {
        Value::Null => Document::Null,
        Value::Bool(value) => Document::Bool(*value),
        Value::Number(number) => Document::Number(match (number.as_u64(), number.as_i64()) {
            (Some(value), _) => Number::PosInt(value),
            (None, Some(value)) => Number::NegInt(value),
            // Without serde_json's arbitrary_precision feature, a number that
            // fits neither integer range is always representable as f64.
            (None, None) => Number::Float(number.as_f64().unwrap_or(0.0)),
        }),
        Value::String(value) => Document::String(value.clone()),
        Value::Array(values) => Document::Array(values.iter().map(document_from_value).collect()),
        Value::Object(entries) => Document::Object(
            entries
                .iter()
                .map(|(key, value)| (key.clone(), document_from_value(value)))
                .collect(),
        ),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum DecodedEvent {
    OutputText(String),
    Refusal(String),
    MessageStopped,
    /// `messageStop` with a recoverable stop reason; the terminal usage
    /// still follows in `metadata`.
    MessageIncomplete(IncompleteReason),
    Usage(ProviderUsage),
    ToolCallStarted {
        index: i32,
        id: String,
        name: String,
    },
    ToolCallArguments {
        index: i32,
        json: String,
    },
    BlockStopped {
        index: i32,
    },
    Ignored,
}

/// Attributes streamed tool-call events to ids by content-block index.
#[derive(Debug, Default)]
struct ToolCallTracker {
    calls: HashMap<i32, String>,
}

impl ToolCallTracker {
    fn start(&mut self, index: i32, id: String) -> Result<(), ProviderError> {
        match self.calls.entry(index) {
            Entry::Occupied(_) => Err(ProviderError::Protocol(
                "Amazon Bedrock stream reused a tool content-block index".to_owned(),
            )),
            Entry::Vacant(entry) => {
                entry.insert(id);
                Ok(())
            }
        }
    }

    fn arguments(&self, index: i32) -> Result<&str, ProviderError> {
        self.calls.get(&index).map(String::as_str).ok_or_else(|| {
            ProviderError::Protocol(
                "Amazon Bedrock stream sent arguments for an unknown tool call".to_owned(),
            )
        })
    }

    fn stop(&mut self, index: i32) -> Option<String> {
        self.calls.remove(&index)
    }
}

fn decode_stream_event(event: ConverseStreamOutput) -> Result<DecodedEvent, ProviderError> {
    match event {
        ConverseStreamOutput::ContentBlockDelta(event) => {
            let index = event.content_block_index;
            match event.delta {
                Some(ContentBlockDelta::Text(text)) => Ok(DecodedEvent::OutputText(text)),
                Some(ContentBlockDelta::ToolUse(delta)) => Ok(DecodedEvent::ToolCallArguments {
                    index,
                    json: delta.input,
                }),
                Some(ContentBlockDelta::Citation(_) | ContentBlockDelta::ReasoningContent(_)) => {
                    Ok(DecodedEvent::Ignored)
                }
                Some(ContentBlockDelta::ToolResult(_)) => {
                    Err(unsupported_output("tool result output"))
                }
                Some(ContentBlockDelta::Image(_)) => Err(unsupported_output("image output")),
                Some(delta) if delta.is_unknown() => Err(ProviderError::Protocol(
                    "Amazon Bedrock returned an unknown content block delta".to_owned(),
                )),
                None => Err(ProviderError::Protocol(
                    "Amazon Bedrock content block delta was missing its payload".to_owned(),
                )),
                Some(_) => Err(ProviderError::Protocol(
                    "Amazon Bedrock returned an unsupported content block delta".to_owned(),
                )),
            }
        }
        ConverseStreamOutput::ContentBlockStart(event) => {
            let index = event.content_block_index;
            match event.start {
                Some(ContentBlockStart::ToolUse(start)) => Ok(DecodedEvent::ToolCallStarted {
                    index,
                    id: start.tool_use_id,
                    name: start.name,
                }),
                Some(ContentBlockStart::ToolResult(_)) => {
                    Err(unsupported_output("tool result output"))
                }
                Some(ContentBlockStart::Image(_)) => Err(unsupported_output("image output")),
                Some(start) if start.is_unknown() => Err(ProviderError::Protocol(
                    "Amazon Bedrock returned an unknown content block start".to_owned(),
                )),
                None => Err(ProviderError::Protocol(
                    "Amazon Bedrock content block start was missing its payload".to_owned(),
                )),
                Some(_) => Err(ProviderError::Protocol(
                    "Amazon Bedrock returned an unsupported content block start".to_owned(),
                )),
            }
        }
        ConverseStreamOutput::MessageStop(event) => decode_stop_reason(event.stop_reason()),
        ConverseStreamOutput::Metadata(event) => {
            let usage = event.usage.ok_or_else(|| {
                ProviderError::Protocol("Amazon Bedrock metadata omitted token usage".to_owned())
            })?;
            Ok(DecodedEvent::Usage(provider_usage(&usage)?))
        }
        ConverseStreamOutput::ContentBlockStop(event) => Ok(DecodedEvent::BlockStopped {
            index: event.content_block_index,
        }),
        ConverseStreamOutput::MessageStart(_) => Ok(DecodedEvent::Ignored),
        event if event.is_unknown() => Err(ProviderError::Protocol(
            "Amazon Bedrock returned an unknown stream event".to_owned(),
        )),
        _ => Err(ProviderError::Protocol(
            "Amazon Bedrock returned an unsupported stream event".to_owned(),
        )),
    }
}

fn decode_stop_reason(reason: &StopReason) -> Result<DecodedEvent, ProviderError> {
    match reason {
        StopReason::EndTurn | StopReason::StopSequence | StopReason::ToolUse => {
            Ok(DecodedEvent::MessageStopped)
        }
        StopReason::ContentFiltered => Ok(DecodedEvent::Refusal(
            "Amazon Bedrock filtered the response".to_owned(),
        )),
        StopReason::GuardrailIntervened => Ok(DecodedEvent::Refusal(
            "Amazon Bedrock guardrail intervened".to_owned(),
        )),
        StopReason::MaxTokens => Ok(DecodedEvent::MessageIncomplete(
            IncompleteReason::OutputTokens,
        )),
        StopReason::ModelContextWindowExceeded => Err(ProviderError::ResponseFailed {
            kind: ProviderErrorKind::ContextExceeded,
            message: "Amazon Bedrock exceeded the model context window".to_owned(),
        }),
        StopReason::MalformedModelOutput => Err(ProviderError::ResponseFailed {
            kind: ProviderErrorKind::Response,
            message: "Amazon Bedrock reported malformed model output".to_owned(),
        }),
        StopReason::MalformedToolUse => Err(ProviderError::ResponseFailed {
            kind: ProviderErrorKind::Response,
            message: "Amazon Bedrock reported malformed tool use".to_owned(),
        }),
        _ => Err(ProviderError::Protocol(
            "Amazon Bedrock returned an unsupported stop reason".to_owned(),
        )),
    }
}

fn provider_usage(usage: &TokenUsage) -> Result<ProviderUsage, ProviderError> {
    let input_tokens = u64::try_from(usage.input_tokens()).map_err(|_| {
        ProviderError::Protocol("Amazon Bedrock returned negative input token usage".to_owned())
    })?;
    let output_tokens = u64::try_from(usage.output_tokens()).map_err(|_| {
        ProviderError::Protocol("Amazon Bedrock returned negative output token usage".to_owned())
    })?;
    let cache_read_input_tokens = u64::try_from(usage.cache_read_input_tokens().unwrap_or(0))
        .map_err(|_| {
            ProviderError::Protocol(
                "Amazon Bedrock returned negative cache-read token usage".to_owned(),
            )
        })?;
    let cache_write_input_tokens = u64::try_from(usage.cache_write_input_tokens().unwrap_or(0))
        .map_err(|_| {
            ProviderError::Protocol(
                "Amazon Bedrock returned negative cache-write token usage".to_owned(),
            )
        })?;
    Ok(ProviderUsage {
        input_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens,
        output_tokens,
        reasoning_tokens: None,
    })
}

fn unsupported_output(kind: &str) -> ProviderError {
    ProviderError::ResponseFailed {
        kind: ProviderErrorKind::Response,
        message: format!("Amazon Bedrock returned unsupported {kind}"),
    }
}

const fn converse_error_kind(error: &ConverseStreamError) -> ProviderErrorKind {
    match error {
        ConverseStreamError::AccessDeniedException(_) => ProviderErrorKind::Authentication,
        ConverseStreamError::ThrottlingException(_) => ProviderErrorKind::RateLimited,
        ConverseStreamError::ResourceNotFoundException(_)
        | ConverseStreamError::ValidationException(_) => ProviderErrorKind::InvalidRequest,
        ConverseStreamError::InternalServerException(_)
        | ConverseStreamError::ModelErrorException(_)
        | ConverseStreamError::ModelNotReadyException(_)
        | ConverseStreamError::ModelStreamErrorException(_)
        | ConverseStreamError::ModelTimeoutException(_)
        | ConverseStreamError::ServiceUnavailableException(_) => ProviderErrorKind::Unavailable,
        _ => ProviderErrorKind::Response,
    }
}

const fn output_error_kind(error: &ConverseStreamOutputError) -> ProviderErrorKind {
    match error {
        ConverseStreamOutputError::ThrottlingException(_) => ProviderErrorKind::RateLimited,
        ConverseStreamOutputError::ValidationException(_) => ProviderErrorKind::InvalidRequest,
        ConverseStreamOutputError::InternalServerException(_)
        | ConverseStreamOutputError::ModelStreamErrorException(_)
        | ConverseStreamOutputError::ServiceUnavailableException(_) => {
            ProviderErrorKind::Unavailable
        }
        _ => ProviderErrorKind::Response,
    }
}

#[derive(Debug)]
struct ResponseBodyLimit {
    max_bytes: usize,
    exceeded: Arc<AtomicBool>,
}

impl ResponseBodyLimit {
    fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            exceeded: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl Intercept for ResponseBodyLimit {
    fn name(&self) -> &'static str {
        "ResponseBodyLimit"
    }

    fn modify_before_deserialization(
        &self,
        context: &mut BeforeDeserializationInterceptorContextMut<'_>,
        _runtime_components: &RuntimeComponents,
        _config: &mut ConfigBag,
    ) -> Result<(), BoxError> {
        let response = context.response_mut();
        let body = response.take_body();
        *response.body_mut() = SdkBody::from_body_1_x(LimitedBody {
            body: Box::pin(body),
            remaining: self.max_bytes,
            exceeded: Arc::clone(&self.exceeded),
        });
        Ok(())
    }
}

struct LimitedBody {
    body: Pin<Box<SdkBody>>,
    remaining: usize,
    exceeded: Arc<AtomicBool>,
}

impl Body for LimitedBody {
    type Data = <SdkBody as Body>::Data;
    type Error = aws_smithy_types::body::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        match self.body.as_mut().poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                let bytes = frame.data_ref().map_or(0, Bytes::len);
                if bytes > self.remaining {
                    self.exceeded.store(true, Ordering::Relaxed);
                    return Poll::Ready(Some(Err(Box::new(ResponseBodyLimitExceeded))));
                }
                self.remaining -= bytes;
                Poll::Ready(Some(Ok(frame)))
            }
            other => other,
        }
    }
}

#[derive(Debug)]
struct ResponseBodyLimitExceeded;

impl fmt::Display for ResponseBodyLimitExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Amazon Bedrock response exceeded the configured wire size limit")
    }
}

impl Error for ResponseBodyLimitExceeded {}

fn request_error<R>(
    error: &SdkError<ConverseStreamError, R>,
    redactions: &[String],
    body_limit_exceeded: bool,
) -> ProviderError
where
    R: fmt::Debug,
{
    if body_limit_exceeded {
        return ProviderError::Protocol(
            "Amazon Bedrock response exceeded the configured wire size limit".to_owned(),
        );
    }
    let message = sdk_error_message(error, redactions);
    match error {
        SdkError::ConstructionFailure(_) => ProviderError::Configuration(message),
        SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) => {
            ProviderError::Transport(message)
        }
        SdkError::ResponseError(_) => ProviderError::Protocol(message),
        SdkError::ServiceError(error) => ProviderError::ResponseFailed {
            kind: converse_error_kind(error.err()),
            message,
        },
        _ => ProviderError::Transport(message),
    }
}

fn stream_error<R>(
    error: &SdkError<ConverseStreamOutputError, R>,
    redactions: &[String],
    body_limit_exceeded: bool,
) -> ProviderError
where
    R: fmt::Debug,
{
    if body_limit_exceeded {
        return ProviderError::Protocol(
            "Amazon Bedrock response exceeded the configured wire size limit".to_owned(),
        );
    }
    let message = sdk_error_message(error, redactions);
    match error {
        SdkError::TimeoutError(_) | SdkError::DispatchFailure(_) => {
            ProviderError::Transport(message)
        }
        SdkError::ConstructionFailure(_) | SdkError::ResponseError(_) => {
            ProviderError::Protocol(message)
        }
        SdkError::ServiceError(error) => ProviderError::ResponseFailed {
            kind: output_error_kind(error.err()),
            message,
        },
        _ => ProviderError::Protocol(message),
    }
}

fn sdk_error_message<E>(error: &E, redactions: &[String]) -> String
where
    E: Error,
{
    sanitize_message(&DisplayErrorContext(error).to_string(), redactions)
}

fn check_stream_event_size(
    event: &ConverseStreamOutput,
    limit: usize,
) -> Result<(), ProviderError> {
    let debug_bytes = bounded_debug_size(event, limit)?;
    let event_bytes = debug_bytes
        .checked_add(EVENT_FRAME_OVERHEAD_BYTES)
        .ok_or_else(|| {
            ProviderError::Protocol("Amazon Bedrock event size overflowed".to_owned())
        })?;
    if event_bytes > limit {
        return Err(ProviderError::Protocol(
            "Amazon Bedrock event exceeded the configured size limit".to_owned(),
        ));
    }
    Ok(())
}

fn bounded_debug_size(value: &impl fmt::Debug, limit: usize) -> Result<usize, ProviderError> {
    let mut counter = BoundedLength { length: 0, limit };
    write!(&mut counter, "{value:?}").map_err(|_| {
        ProviderError::Protocol(
            "Amazon Bedrock event exceeded the configured size limit".to_owned(),
        )
    })?;
    Ok(counter.length)
}

struct BoundedLength {
    length: usize,
    limit: usize,
}

impl fmt::Write for BoundedLength {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        self.length = self.length.checked_add(value.len()).ok_or(fmt::Error)?;
        if self.length > self.limit {
            return Err(fmt::Error);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use aws_config::Region;
    use aws_sdk_bedrockruntime::types::{
        ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent,
        ConverseStreamMetadataEvent, MessageStopEvent, ToolUseBlockDelta, ToolUseBlockStart,
    };
    use serde_json::json;

    use super::*;
    use crate::{Message, ToolSpec};

    #[test]
    fn constructor_is_network_free_and_debug_redacts_api_keys() {
        let auth = BedrockAuth::ApiKey("bedrock-test-secret".into());
        let provider = Bedrock::new(auth.clone(), Some("us-east-1".to_owned())).unwrap();
        let default_provider = Bedrock::new(BedrockAuth::DefaultChain, None).unwrap();

        assert!(!format!("{auth:?}").contains("bedrock-test-secret"));
        assert!(!format!("{provider:?}").contains("bedrock-test-secret"));
        drop(default_provider);
    }

    #[tokio::test]
    async fn failed_client_initialization_is_retryable() {
        let client = OnceCell::new();
        let first = client
            .get_or_try_init(|| async { Err::<usize, _>("temporary failure") })
            .await;
        assert_eq!(first.err(), Some("temporary failure"));

        let second = client.get_or_try_init(|| async { Ok::<_, &str>(42) }).await;

        assert_eq!(second, Ok(&42));
        assert_eq!(client.get(), Some(&42));
    }

    #[test]
    fn rejects_invalid_auth_and_region_values() {
        for (auth, region) in [
            (BedrockAuth::ApiKey("".into()), None),
            (BedrockAuth::ApiKey("\r\n".into()), None),
            (BedrockAuth::Profile(" ".to_owned()), None),
            (BedrockAuth::DefaultChain, Some(String::new())),
            (BedrockAuth::DefaultChain, Some("bad\nregion".to_owned())),
            (
                BedrockAuth::ApiKey("bedrock-test-secret".into()),
                Some("attacker.example?x=".to_owned()),
            ),
        ] {
            let error =
                Bedrock::new(auth, region).expect_err("invalid configuration must be rejected");
            assert!(matches!(error, ProviderError::Configuration(_)));
        }
    }

    #[test]
    fn maps_messages_roles_and_max_output_tokens() {
        let request = ModelRequest::new(
            "anthropic.claude-test",
            vec![Message::user("hello"), Message::assistant("hi")],
            512,
        );
        let mapped = ConverseRequest::try_from(&request).unwrap();

        assert_eq!(mapped.model_id, "anthropic.claude-test");
        assert_eq!(mapped.messages.len(), 2);
        assert_eq!(mapped.messages[0].role(), &ConversationRole::User);
        assert_eq!(mapped.messages[1].role(), &ConversationRole::Assistant);
        assert_eq!(mapped.messages[0].content()[0].as_text().unwrap(), "hello");
        assert_eq!(mapped.messages[1].content()[0].as_text().unwrap(), "hi");
        assert_eq!(mapped.inference_config.max_tokens(), Some(512));
        assert!(mapped.tool_config.is_none());
        assert!(mapped.system.is_none());
    }

    #[test]
    fn maps_the_system_prompt_to_converse_system_blocks() {
        let request = ModelRequest::new("anthropic.claude-test", vec![Message::user("ping")], 64)
            .with_system("You are QQ.");
        let mapped = ConverseRequest::try_from(&request).unwrap();
        let system = mapped.system.unwrap();
        assert_eq!(system.len(), 1);
        assert!(
            matches!(&system[0], SystemContentBlock::Text(text) if text == "You are QQ."),
            "unexpected system block: {system:?}"
        );
    }

    #[test]
    fn maps_tool_declarations_and_tool_history_blocks() {
        let request = ModelRequest::new(
            "anthropic.claude-test",
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
                Message::tool_results(vec![
                    ContentBlock::ToolResult {
                        call_id: "toolu_1".to_owned(),
                        content: "(config)".to_owned(),
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        call_id: "toolu_2".to_owned(),
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
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )]);
        let mapped = ConverseRequest::try_from(&request).unwrap();

        let assistant = mapped.messages[1].content();
        assert_eq!(assistant[0].as_text().unwrap(), "Reading it now.");
        let tool_use = assistant[1].as_tool_use().unwrap();
        assert_eq!(tool_use.tool_use_id(), "toolu_1");
        assert_eq!(tool_use.name(), "read_file");
        assert_eq!(
            tool_use.input(),
            &Document::Object(HashMap::from([(
                "path".to_owned(),
                Document::String("config.ron".to_owned()),
            )]))
        );

        assert_eq!(mapped.messages[2].role(), &ConversationRole::User);
        let results = mapped.messages[2].content();
        let success = results[0].as_tool_result().unwrap();
        assert_eq!(success.tool_use_id(), "toolu_1");
        assert_eq!(success.content()[0].as_text().unwrap(), "(config)");
        assert_eq!(success.status(), None);
        let failure = results[1].as_tool_result().unwrap();
        assert_eq!(failure.tool_use_id(), "toolu_2");
        assert_eq!(failure.content()[0].as_text().unwrap(), "denied");
        assert_eq!(failure.status(), Some(&ToolResultStatus::Error));

        let tools = mapped.tool_config.unwrap();
        let specification = tools.tools()[0].as_tool_spec().unwrap();
        assert_eq!(specification.name(), "read_file");
        assert_eq!(specification.description(), Some("Reads one file"));
        assert_eq!(
            specification.input_schema().unwrap().as_json().unwrap(),
            &document_from_value(
                &json!({"type": "object", "properties": {"path": {"type": "string"}}})
            )
        );
    }

    #[test]
    fn converts_json_values_to_smithy_documents() {
        let value = json!({
            "text": "path",
            "count": 3,
            "offset": -7,
            "ratio": 0.5,
            "flag": true,
            "missing": null,
            "items": [1, "two"],
        });

        assert_eq!(
            document_from_value(&value),
            Document::Object(HashMap::from([
                ("text".to_owned(), Document::String("path".to_owned())),
                ("count".to_owned(), Document::Number(Number::PosInt(3))),
                ("offset".to_owned(), Document::Number(Number::NegInt(-7))),
                ("ratio".to_owned(), Document::Number(Number::Float(0.5))),
                ("flag".to_owned(), Document::Bool(true)),
                ("missing".to_owned(), Document::Null),
                (
                    "items".to_owned(),
                    Document::Array(vec![
                        Document::Number(Number::PosInt(1)),
                        Document::String("two".to_owned()),
                    ]),
                ),
            ]))
        );
    }

    #[test]
    fn rejects_max_output_tokens_that_do_not_fit_bedrock() {
        let request = ModelRequest::new("model", vec![Message::user("hello")], u32::MAX);
        let error = ConverseRequest::try_from(&request)
            .expect_err("out-of-range token count must be rejected");

        assert!(matches!(error, ProviderError::Configuration(_)));
    }

    #[test]
    fn decodes_text_and_successful_stop_events() {
        let text = ConverseStreamOutput::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .content_block_index(0)
                .delta(ContentBlockDelta::Text("hello".to_owned()))
                .build()
                .unwrap(),
        );
        let stop = |reason| {
            ConverseStreamOutput::MessageStop(
                MessageStopEvent::builder()
                    .stop_reason(reason)
                    .build()
                    .unwrap(),
            )
        };

        assert_eq!(
            decode_stream_event(text).unwrap(),
            DecodedEvent::OutputText("hello".to_owned())
        );
        assert_eq!(
            decode_stream_event(stop(StopReason::EndTurn)).unwrap(),
            DecodedEvent::MessageStopped
        );
        assert_eq!(
            decode_stream_event(stop(StopReason::StopSequence)).unwrap(),
            DecodedEvent::MessageStopped
        );
    }

    #[test]
    fn decodes_metadata_usage_and_rejects_negative_counts() {
        let usage = TokenUsage::builder()
            .input_tokens(12)
            .cache_read_input_tokens(4)
            .cache_write_input_tokens(3)
            .output_tokens(9)
            .total_tokens(28)
            .build()
            .unwrap();
        let metadata = ConverseStreamOutput::Metadata(
            ConverseStreamMetadataEvent::builder().usage(usage).build(),
        );
        assert_eq!(
            decode_stream_event(metadata).unwrap(),
            DecodedEvent::Usage(ProviderUsage {
                input_tokens: 12,
                cache_read_input_tokens: 4,
                cache_write_input_tokens: 3,
                output_tokens: 9,
                reasoning_tokens: None,
            })
        );

        let invalid = TokenUsage::builder()
            .input_tokens(-1)
            .output_tokens(1)
            .total_tokens(0)
            .build()
            .unwrap();
        assert!(matches!(
            provider_usage(&invalid),
            Err(ProviderError::Protocol(_))
        ));
    }

    #[test]
    fn classifies_refusal_incomplete_tool_and_unknown_stops() {
        for reason in [StopReason::ContentFiltered, StopReason::GuardrailIntervened] {
            assert!(matches!(
                decode_stop_reason(&reason).unwrap(),
                DecodedEvent::Refusal(_)
            ));
        }

        assert_eq!(
            decode_stop_reason(&StopReason::MaxTokens).unwrap(),
            DecodedEvent::MessageIncomplete(IncompleteReason::OutputTokens)
        );
        assert!(matches!(
            decode_stop_reason(&StopReason::ModelContextWindowExceeded),
            Err(ProviderError::ResponseFailed {
                kind: ProviderErrorKind::ContextExceeded,
                ..
            })
        ));

        assert_eq!(
            decode_stop_reason(&StopReason::ToolUse).unwrap(),
            DecodedEvent::MessageStopped
        );
        let unknown_error = decode_stop_reason(&StopReason::from("future_reason")).unwrap_err();
        assert!(matches!(unknown_error, ProviderError::Protocol(_)));
    }

    #[test]
    fn decodes_tool_call_stream_events() {
        let started = ConverseStreamOutput::ContentBlockStart(
            ContentBlockStartEvent::builder()
                .content_block_index(1)
                .start(ContentBlockStart::ToolUse(
                    ToolUseBlockStart::builder()
                        .tool_use_id("toolu_1")
                        .name("read_file")
                        .build()
                        .unwrap(),
                ))
                .build()
                .unwrap(),
        );
        let arguments = ConverseStreamOutput::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .content_block_index(1)
                .delta(ContentBlockDelta::ToolUse(
                    ToolUseBlockDelta::builder()
                        .input("{\"path\":")
                        .build()
                        .unwrap(),
                ))
                .build()
                .unwrap(),
        );
        let stopped = ConverseStreamOutput::ContentBlockStop(
            ContentBlockStopEvent::builder()
                .content_block_index(1)
                .build()
                .unwrap(),
        );
        let stop = ConverseStreamOutput::MessageStop(
            MessageStopEvent::builder()
                .stop_reason(StopReason::ToolUse)
                .build()
                .unwrap(),
        );

        assert_eq!(
            decode_stream_event(started).unwrap(),
            DecodedEvent::ToolCallStarted {
                index: 1,
                id: "toolu_1".to_owned(),
                name: "read_file".to_owned(),
            }
        );
        assert_eq!(
            decode_stream_event(arguments).unwrap(),
            DecodedEvent::ToolCallArguments {
                index: 1,
                json: "{\"path\":".to_owned(),
            }
        );
        assert_eq!(
            decode_stream_event(stopped).unwrap(),
            DecodedEvent::BlockStopped { index: 1 }
        );
        assert_eq!(
            decode_stream_event(stop).unwrap(),
            DecodedEvent::MessageStopped
        );
    }

    #[test]
    fn attributes_tool_calls_by_index_and_rejects_unknown_or_reused_indexes() {
        let mut tracker = ToolCallTracker::default();
        tracker.start(1, "toolu_1".to_owned()).unwrap();

        assert_eq!(tracker.arguments(1).unwrap(), "toolu_1");
        let unknown = tracker.arguments(4).unwrap_err();
        assert!(matches!(unknown, ProviderError::Protocol(_)));
        let reused = tracker.start(1, "toolu_2".to_owned()).unwrap_err();
        assert!(matches!(reused, ProviderError::Protocol(_)));
        assert_eq!(tracker.stop(1), Some("toolu_1".to_owned()));
        assert_eq!(tracker.stop(1), None);
    }

    #[test]
    fn enforces_output_and_event_size_limits() {
        let event = ConverseStreamOutput::ContentBlockDelta(
            ContentBlockDeltaEvent::builder()
                .content_block_index(0)
                .delta(ContentBlockDelta::Text("oversized".to_owned()))
                .build()
                .unwrap(),
        );
        let event_error = check_stream_event_size(&event, 8).unwrap_err();
        let mut output = ByteCounter::new(8, "output overflow", "output limit");
        let output_error = output.add(9).unwrap_err();

        assert!(matches!(event_error, ProviderError::Protocol(_)));
        assert!(matches!(output_error, ProviderError::Protocol(_)));
    }

    #[tokio::test]
    async fn enforces_the_raw_response_body_limit() {
        let exceeded = Arc::new(AtomicBool::new(false));
        let mut body = LimitedBody {
            body: Box::pin(SdkBody::from("too large")),
            remaining: 4,
            exceeded: Arc::clone(&exceeded),
        };
        let frame = std::future::poll_fn(|context| Pin::new(&mut body).poll_frame(context))
            .await
            .expect("body should return one frame");

        assert!(frame.is_err());
        assert!(exceeded.load(Ordering::Relaxed));
    }

    #[test]
    fn disables_sdk_retries_and_selects_the_requested_auth_scheme() {
        let shared_config = SdkConfig::builder()
            .region(Region::new("us-east-1"))
            .build();
        let aws = service_config(&shared_config, None);
        let api_key = service_config(&shared_config, Some("bedrock-test-secret".to_owned()));

        assert_eq!(aws.retry_config().unwrap().max_attempts(), 1);
        assert_eq!(api_key.retry_config().unwrap().max_attempts(), 1);
        assert!(format!("{:?}", aws.auth_scheme_preference()).contains("sigv4"));
        assert!(format!("{:?}", api_key.auth_scheme_preference()).contains("httpBearerAuth"));
    }

    #[test]
    fn sanitizes_secrets_controls_and_long_error_messages() {
        let message = format!("bedrock-test-secret\n{}", "x".repeat(2_000));
        let sanitized = sanitize_message(&message, &["bedrock-test-secret".to_owned()]);

        assert!(!sanitized.contains("bedrock-test-secret"));
        assert!(!sanitized.contains('\n'));
        assert!(sanitized.contains("[REDACTED]"));
        assert!(sanitized.chars().count() <= crate::sanitize::ERROR_MESSAGE_CHARS_LIMIT);
    }
}
