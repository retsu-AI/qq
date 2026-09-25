//! Provider-neutral request, event, usage, and error vocabulary.

use std::sync::{Arc, OnceLock};

use qq_reasoning::{ReasoningEffort, ReasoningKind};
use serde_json::value::RawValue;
use thiserror::Error;

/// A provider-neutral model generation request.
///
/// The transcript is shared with its owner: a run holds the same messages
/// across turns and appends to them, so the request borrows the list by
/// reference count and the per-attempt clone every adapter performs inside
/// its restart loop copies nothing. Cloning a request is cheap.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    model: Arc<str>,
    messages: Arc<Vec<Message>>,
    tools: Arc<[ToolSpec]>,
    system: Option<Arc<str>>,
    max_output_tokens: u32,
    reasoning_effort: Option<ReasoningEffort>,
}

impl ModelRequest {
    #[must_use]
    pub fn new(
        model: impl Into<Arc<str>>,
        messages: impl Into<Arc<Vec<Message>>>,
        max_output_tokens: u32,
    ) -> Self {
        Self {
            model: model.into(),
            messages: messages.into(),
            tools: Arc::from([]),
            system: None,
            max_output_tokens,
            reasoning_effort: None,
        }
    }

    /// Declares the tools the model may call during this request. The list
    /// is shared, not copied: a run declares the same catalog on every turn
    /// and only the request carrying it is per-turn.
    #[must_use]
    pub fn with_tools(mut self, tools: impl Into<Arc<[ToolSpec]>>) -> Self {
        self.tools = tools.into();
        self
    }

    /// Sets the system prompt; each codec maps it to its native
    /// system/instructions slot. Requests without one keep their previous
    /// wire shape.
    #[must_use]
    pub fn with_system(mut self, system: impl Into<Arc<str>>) -> Self {
        self.system = Some(system.into());
        self
    }

    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    #[must_use]
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    /// The shared transcript handle, for a caller that appends to the same
    /// list after this request completes without copying it.
    #[must_use]
    pub fn shared_messages(&self) -> &Arc<Vec<Message>> {
        &self.messages
    }

    #[must_use]
    pub fn tools(&self) -> &[ToolSpec] {
        &self.tools
    }

    #[must_use]
    pub fn system(&self) -> Option<&str> {
        self.system.as_deref()
    }

    #[must_use]
    pub const fn max_output_tokens(&self) -> u32 {
        self.max_output_tokens
    }

    /// Requests a provider-native reasoning effort. The selected adapter
    /// validates whether it can encode the value before transport.
    #[must_use]
    pub const fn with_reasoning_effort(mut self, effort: ReasoningEffort) -> Self {
        self.reasoning_effort = match effort {
            ReasoningEffort::Default => None,
            effort => Some(effort),
        };
        self
    }

    #[must_use]
    pub const fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        self.reasoning_effort
    }

    /// Returns a configuration error for an adapter that cannot encode effort.
    /// This guard runs before transport or request-time credential authorization.
    pub(crate) fn unsupported_reasoning_effort(&self, adapter: &str) -> Option<ProviderError> {
        self.reasoning_effort.map(|effort| {
            ProviderError::Configuration(format!(
                "reasoning effort `{effort:?}` is unsupported by {adapter}"
            ))
        })
    }

    /// A lower bound on the encoded request body, from the payload bytes the
    /// wire codecs embed verbatim plus fixed per-item framing. Used to size
    /// the body buffer once instead of doubling through a megabyte.
    #[must_use]
    pub fn wire_size_hint(&self) -> usize {
        const MESSAGE_FRAMING: usize = 32;
        const BLOCK_FRAMING: usize = 64;
        const TOOL_FRAMING: usize = 64;
        let messages = self.messages.iter().fold(0, |total, message| {
            message.content().iter().fold(
                total + MESSAGE_FRAMING + message.replay().map_or(0, str::len),
                |total, block| {
                    total
                        + BLOCK_FRAMING
                        + match block {
                            ContentBlock::Text { text } => text.len(),
                            ContentBlock::ToolCall {
                                id,
                                name,
                                arguments,
                            } => id.len() + name.len() + arguments.get().len(),
                            ContentBlock::ToolResult {
                                call_id, content, ..
                            } => call_id.len() + content.len(),
                        }
                },
            )
        });
        let tools = self.tools.iter().fold(0, |total, tool| {
            total
                + TOOL_FRAMING
                + tool.name().len()
                + tool.description().len()
                + tool.input_schema().get().len()
        });
        messages + tools + self.system.as_ref().map_or(0, |system| system.len()) + 256
    }
}

/// A tool the model may call, described provider-neutrally.
///
/// Specs are immutable once built and travel with every request, catalog,
/// and plan, so the payload is shared: cloning is a reference count bump,
/// never a deep copy of the schema. The schema is kept in its compact JSON
/// encoding: every request writes it verbatim, so nothing is re-serialized
/// per turn, and its byte length is the wire cost.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    inner: Arc<ToolSpecInner>,
}

#[derive(Debug)]
struct ToolSpecInner {
    name: String,
    description: String,
    input_schema: Box<RawValue>,
    // Gemini's `Schema` is a restricted OpenAPI subset that rejects unknown
    // JSON Schema keywords with HTTP 400. The reduced form is derived from
    // `input_schema` on the first Google request and cached so the encode
    // hot path stays parse-free; it is never part of equality.
    gemini_schema: OnceLock<Box<RawValue>>,
}

// `RawValue` has no `PartialEq`; two schemas are equal when their compact
// encodings are, which is exact for text `serde_json` produced (sorted keys).
impl PartialEq for ToolSpecInner {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
            && self.description == other.description
            && self.input_schema.get() == other.input_schema.get()
    }
}

/// JSON Schema keywords absent from Gemini's `Schema`; any of these anywhere
/// in a tool's parameters fails the whole request.
const GEMINI_UNSUPPORTED_SCHEMA_KEYWORDS: &[&str] = &[
    "additionalProperties",
    "$schema",
    "$id",
    "$ref",
    "$defs",
    "$comment",
    "definitions",
    "oneOf",
    "allOf",
    "not",
    "const",
    "examples",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "patternProperties",
    "additionalItems",
    "dependencies",
    "if",
    "then",
    "else",
    "contentMediaType",
    "contentEncoding",
    "readOnly",
    "writeOnly",
    "deprecated",
];

/// Removes every unsupported keyword from a schema and from the schemas it
/// nests under `properties`, `items`, and `anyOf`. Keys of the `properties`
/// map are property names, not keywords, so they are never removed; values
/// such as `default`, `enum`, and `example` are data and left untouched.
fn strip_gemini_unsupported_keywords(schema: &mut serde_json::Value) {
    let serde_json::Value::Object(object) = schema else {
        return;
    };
    object.retain(|key, _| !GEMINI_UNSUPPORTED_SCHEMA_KEYWORDS.contains(&key.as_str()));
    for (key, value) in object.iter_mut() {
        match key.as_str() {
            "properties" => {
                if let serde_json::Value::Object(properties) = value {
                    for property in properties.values_mut() {
                        strip_gemini_unsupported_keywords(property);
                    }
                }
            }
            "items" | "anyOf" => match value {
                serde_json::Value::Array(schemas) => {
                    for nested in schemas {
                        strip_gemini_unsupported_keywords(nested);
                    }
                }
                serde_json::Value::Object(_) => strip_gemini_unsupported_keywords(value),
                serde_json::Value::Null
                | serde_json::Value::Bool(_)
                | serde_json::Value::Number(_)
                | serde_json::Value::String(_) => {}
            },
            _ => {}
        }
    }
}

impl ToolSpec {
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: serde_json::Value,
    ) -> Self {
        // A `Value` always has a JSON encoding; `to_raw_value` fails only on
        // a non-string map key, which `Value` cannot hold.
        let input_schema = serde_json::value::to_raw_value(&input_schema)
            .expect("a serde_json::Value always encodes as JSON");
        Self::from_raw(name, description, input_schema)
    }

    /// A spec whose schema is already compact JSON text (an MCP server's
    /// declaration, a persisted catalog entry). No parse, no re-encode.
    #[must_use]
    pub fn from_raw(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Box<RawValue>,
    ) -> Self {
        Self {
            inner: Arc::new(ToolSpecInner {
                name: name.into(),
                description: description.into(),
                input_schema,
                gemini_schema: OnceLock::new(),
            }),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    #[must_use]
    pub fn description(&self) -> &str {
        &self.inner.description
    }

    /// The schema as compact JSON. Serializes verbatim; `.get()` is the text.
    #[must_use]
    pub fn input_schema(&self) -> &RawValue {
        &self.inner.input_schema
    }

    /// The schema reduced to Gemini's `Schema` subset, computed once per spec
    /// and shared by every clone. Only the Google codec consumes this; every
    /// other provider keeps `input_schema` verbatim.
    pub(crate) fn gemini_parameters(&self) -> &RawValue {
        self.inner.gemini_schema.get_or_init(|| {
            let original = self.inner.input_schema.get();
            match serde_json::from_str::<serde_json::Value>(original) {
                Ok(mut schema) => {
                    strip_gemini_unsupported_keywords(&mut schema);
                    // A `Value` always has a JSON encoding (see `ToolSpec::new`).
                    serde_json::value::to_raw_value(&schema)
                        .expect("a serde_json::Value always encodes as JSON")
                }
                // `from_raw` accepted a `RawValue`, which `serde_json` already
                // validated as JSON; an unparsable schema cannot be improved
                // here, so send it as declared and let the provider report it.
                Err(_) => self.inner.input_schema.clone(),
            }
        })
    }
}

/// A message in model context, holding ordered content blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    role: Role,
    content: Vec<ContentBlock>,
    replay: Option<Arc<str>>,
}

impl Message {
    #[must_use]
    pub fn new(role: Role, content: Vec<ContentBlock>) -> Self {
        Self {
            role,
            content,
            replay: None,
        }
    }

    /// A user message with one text block.
    #[must_use]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            replay: None,
            content: vec![ContentBlock::Text {
                text: content.into(),
            }],
        }
    }

    /// An assistant message with one text block.
    #[must_use]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            replay: None,
            content: vec![ContentBlock::Text {
                text: content.into(),
            }],
        }
    }

    /// A user-role message carrying tool results back to the model.
    #[must_use]
    pub fn tool_results(results: Vec<ContentBlock>) -> Self {
        Self {
            role: Role::User,
            replay: None,
            content: results,
        }
    }

    #[must_use]
    pub const fn role(&self) -> Role {
        self.role
    }

    #[must_use]
    pub fn content(&self) -> &[ContentBlock] {
        &self.content
    }

    /// Opaque provider continuation data, separate from user-visible content.
    #[must_use]
    pub fn with_replay(mut self, replay: Arc<str>) -> Self {
        self.replay = Some(replay);
        self
    }

    #[must_use]
    pub fn replay(&self) -> Option<&str> {
        self.replay.as_deref()
    }

    /// Whether any block carries usable content.
    #[must_use]
    pub fn has_content(&self) -> bool {
        self.content.iter().any(|block| match block {
            ContentBlock::Text { text } => !text.trim().is_empty(),
            ContentBlock::ToolCall { .. } | ContentBlock::ToolResult { .. } => true,
        })
    }
}

/// One ordered unit of message content.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// A model-requested tool invocation, valid in assistant messages. The
    /// arguments are kept as the compact JSON text the run validated: wire
    /// codecs that want an object embed it verbatim and codecs that want a
    /// string send `.get()`, so history is never re-serialized per request.
    ToolCall {
        id: String,
        name: String,
        arguments: Box<RawValue>,
    },
    /// The result of one tool invocation, valid in user messages.
    ToolResult {
        call_id: String,
        content: String,
        is_error: bool,
    },
}

impl PartialEq for ContentBlock {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Text { text }, Self::Text { text: other }) => text == other,
            (
                Self::ToolCall {
                    id,
                    name,
                    arguments,
                },
                Self::ToolCall {
                    id: other_id,
                    name: other_name,
                    arguments: other_arguments,
                },
            ) => id == other_id && name == other_name && arguments.get() == other_arguments.get(),
            (
                Self::ToolResult {
                    call_id,
                    content,
                    is_error,
                },
                Self::ToolResult {
                    call_id: other_call_id,
                    content: other_content,
                    is_error: other_is_error,
                },
            ) => call_id == other_call_id && content == other_content && is_error == other_is_error,
            (Self::Text { .. } | Self::ToolCall { .. } | Self::ToolResult { .. }, _) => false,
        }
    }
}

impl ContentBlock {
    /// A tool-call block from a parsed argument value; for fixtures and for
    /// callers that build arguments structurally.
    #[must_use]
    pub fn tool_call(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: &serde_json::Value,
    ) -> Self {
        Self::ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: serde_json::value::to_raw_value(arguments)
                .expect("a serde_json::Value always encodes as JSON"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// Events common to provider streaming implementations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderEvent {
    /// Complete opaque continuation data, emitted only for a validated turn.
    Replay {
        data: Arc<str>,
    },
    OutputTextDelta {
        text: String,
    },
    RefusalDelta {
        text: String,
    },
    ReasoningStarted {
        kind: ReasoningKind,
    },
    ReasoningDelta {
        kind: ReasoningKind,
        text: String,
    },
    ReasoningCompleted {
        kind: ReasoningKind,
    },
    ToolCallStarted {
        id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        id: String,
        json: String,
    },
    ToolCallCompleted {
        id: String,
    },
    Completed {
        usage: Option<ProviderUsage>,
    },
    /// The provider stopped generating before the model finished its turn
    /// for a reason the caller can recover from by asking it to continue.
    /// Text streamed so far is valid; tool calls still open when this arrives
    /// carry incomplete arguments and must not be executed. Terminal like
    /// `Completed`. Stops the caller cannot recover from (content filters,
    /// refusals) remain `ProviderError::ResponseIncomplete`.
    Incomplete {
        usage: Option<ProviderUsage>,
        reason: IncompleteReason,
    },
}

/// Why a provider stopped a response short of the model finishing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IncompleteReason {
    /// The response reached the request's output token limit.
    OutputTokens,
    /// The provider paused a long-running turn (Anthropic `pause_turn`) and
    /// expects the caller to resend to resume.
    Paused,
}

impl IncompleteReason {
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::OutputTokens => "the response reached its output token limit",
            Self::Paused => "the provider paused the response before completion",
        }
    }
}

/// Provider-neutral token counts for one completed model response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderUsage {
    /// Input tokens that were neither read from nor written to a cache.
    pub input_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_write_input_tokens: u64,
    /// All generated tokens, including hidden reasoning tokens when reported.
    pub output_tokens: u64,
    /// The reasoning-token portion of `output_tokens` when the provider
    /// reports it separately.
    pub reasoning_tokens: Option<u64>,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("provider configuration is invalid: {0}")]
    Configuration(String),
    #[error("provider credentials are unavailable: {0}")]
    CredentialsUnavailable(String),
    #[error("provider request failed: {0}")]
    Transport(String),
    #[error("provider returned HTTP {status}: {message}")]
    Api { status: u16, message: String },
    #[error("provider response failed: {message}")]
    ResponseFailed {
        kind: ProviderErrorKind,
        message: String,
    },
    #[error("provider response was incomplete: {0}")]
    ResponseIncomplete(String),
    #[error("provider stream was invalid: {0}")]
    Protocol(String),
}

impl ProviderError {
    #[must_use]
    pub const fn kind(&self) -> ProviderErrorKind {
        match self {
            Self::Configuration(_) => ProviderErrorKind::Configuration,
            Self::CredentialsUnavailable(_) => ProviderErrorKind::Authentication,
            Self::Transport(_) => ProviderErrorKind::Transport,
            Self::Api { status, .. } => match *status {
                400 | 404 | 409 | 422 => ProviderErrorKind::InvalidRequest,
                401 | 403 => ProviderErrorKind::Authentication,
                429 => ProviderErrorKind::RateLimited,
                500..=599 => ProviderErrorKind::Unavailable,
                _ => ProviderErrorKind::Api,
            },
            Self::ResponseFailed { kind, .. } => *kind,
            Self::ResponseIncomplete(_) => ProviderErrorKind::Response,
            Self::Protocol(_) => ProviderErrorKind::Protocol,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Configuration,
    Authentication,
    RateLimited,
    InvalidRequest,
    /// The request exceeded the model's context window (or the provider's
    /// request-size cap). Distinguished from other invalid requests because
    /// the session layer can recover by compacting and retrying.
    ContextExceeded,
    Unavailable,
    Transport,
    Api,
    Response,
    Protocol,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_shares_the_transcript_and_releases_it_when_dropped() {
        let transcript = Arc::new(vec![Message::user("hello")]);
        let request = ModelRequest::new("m", Arc::clone(&transcript), 16);
        // The adapter's per-attempt clone is a reference-count bump.
        let attempt = request.clone();
        assert_eq!(Arc::strong_count(&transcript), 3);
        assert!(Arc::ptr_eq(attempt.shared_messages(), &transcript));
        drop(attempt);
        drop(request);
        // Once every stream is gone the owner appends in place.
        assert_eq!(Arc::strong_count(&transcript), 1);
    }

    #[test]
    fn tool_call_arguments_and_schemas_keep_their_compact_text() {
        let block = ContentBlock::tool_call(
            "call_1",
            "read_file",
            &serde_json::json!({"path": "a.rs", "n": 1}),
        );
        let ContentBlock::ToolCall { arguments, .. } = &block else {
            panic!("expected a tool call");
        };
        assert_eq!(arguments.get(), r#"{"n":1,"path":"a.rs"}"#);
        assert_eq!(block, block.clone());

        let spec = ToolSpec::new(
            "read_file",
            "Reads",
            serde_json::json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        );
        assert_eq!(
            spec.input_schema().get(),
            r#"{"properties":{"path":{"type":"string"}},"type":"object"}"#
        );
        let raw = ToolSpec::from_raw(
            "read_file",
            "Reads",
            serde_json::value::RawValue::from_string(
                r#"{"properties":{"path":{"type":"string"}},"type":"object"}"#.to_owned(),
            )
            .unwrap(),
        );
        assert_eq!(spec, raw);
    }

    #[test]
    fn gemini_parameters_are_computed_once_and_shared_by_clones() {
        let spec = ToolSpec::new(
            "edit_file",
            "Edits",
            serde_json::json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {"old": {"type": "string", "const": "x"}},
                            "additionalProperties": false,
                        },
                    },
                    "mode": {"oneOf": [{"type": "string"}, {"type": "null"}]},
                },
                "required": ["edits"],
                "additionalProperties": false,
            }),
        );
        let first = spec.gemini_parameters();
        let shared = spec.clone();
        let second = shared.gemini_parameters();
        assert!(std::ptr::eq(first, second));
        assert_eq!(
            first.get(),
            r#"{"properties":{"edits":{"items":{"properties":{"old":{"type":"string"}},"type":"object"},"type":"array"},"mode":{}},"required":["edits"],"type":"object"}"#
        );
        // The provider-neutral schema is untouched, as is equality.
        assert!(spec.input_schema().get().contains("additionalProperties"));
        assert_eq!(spec, spec.clone());
    }

    #[test]
    fn gemini_parameters_keep_a_schema_with_nothing_to_strip() {
        let spec = ToolSpec::new(
            "read_file",
            "Reads",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "file"},
                    "mode": {"type": "string", "enum": ["a", "b"], "default": "a"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 10},
                    "tags": {"type": "array", "items": {"type": "string"}},
                    "either": {"anyOf": [{"type": "string"}, {"type": "integer"}]},
                },
                "required": ["path"],
            }),
        );
        assert_eq!(spec.gemini_parameters().get(), spec.input_schema().get());
    }

    #[test]
    fn unsupported_effort_helper_constructs_an_adapter_specific_error() {
        let request = ModelRequest::new("m", vec![Message::user("hello")], 16)
            .with_reasoning_effort(ReasoningEffort::Xhigh);
        assert!(matches!(
            request.unsupported_reasoning_effort("Anthropic Messages"),
            Some(ProviderError::Configuration(message))
                if message.contains("unsupported by Anthropic Messages")
        ));
    }
}
