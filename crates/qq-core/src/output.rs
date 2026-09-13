//! Typed final output: a bounded JSON Schema subset compiled once per run at
//! admission and evaluated against the final answer at the completion
//! boundary. No references, no remote fetches, no regular expressions: every
//! accepted keyword validates in time linear in the instance.

use std::{collections::BTreeMap, fmt::Write as _, sync::Arc};

use qq_protocol::{
    FinalOutput, MAX_OUTPUT_ERROR_BYTES, MAX_OUTPUT_REPAIR_TURNS, MAX_OUTPUT_SCHEMA_BYTES,
    MAX_OUTPUT_SCHEMA_DEPTH, MAX_OUTPUT_SCHEMA_VALUES, OutputContract,
};
use serde_json::Value;
use thiserror::Error;

/// Most validation failures reported for one answer.
pub const MAX_OUTPUT_ERRORS: usize = 16;

/// Keywords the subset understands. Anything else in a schema object is a
/// compile error so a caller learns at admission, not from an answer that
/// "validated" against a keyword the runtime ignored.
const SUPPORTED_KEYWORDS: &[&str] = &[
    "$schema",
    "$id",
    "$comment",
    "title",
    "description",
    "default",
    "examples",
    "type",
    "enum",
    "const",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "minProperties",
    "maxProperties",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
];

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OutputSchemaError {
    #[error("output schema is {actual} bytes; the limit is {limit}")]
    TooLarge { actual: usize, limit: usize },
    #[error("output schema nests {actual} levels deep; the limit is {limit}")]
    TooDeep { actual: usize, limit: usize },
    #[error("output schema holds more than {limit} JSON values")]
    TooManyValues { limit: usize },
    #[error("output repair turns must be at most {limit}, got {actual}")]
    TooManyRepairTurns { actual: u8, limit: u8 },
    #[error("output schema at {pointer}: references are not supported")]
    Reference { pointer: String },
    #[error("output schema at {pointer}: unsupported keyword {keyword:?}")]
    UnsupportedKeyword { pointer: String, keyword: String },
    #[error("output schema at {pointer}: {message}")]
    Invalid { pointer: String, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonType {
    Null,
    Boolean,
    Object,
    Array,
    Number,
    Integer,
    String,
}

impl JsonType {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "null" => Self::Null,
            "boolean" => Self::Boolean,
            "object" => Self::Object,
            "array" => Self::Array,
            "number" => Self::Number,
            "integer" => Self::Integer,
            "string" => Self::String,
            _ => return None,
        })
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Boolean => "boolean",
            Self::Object => "object",
            Self::Array => "array",
            Self::Number => "number",
            Self::Integer => "integer",
            Self::String => "string",
        }
    }

    fn matches(self, value: &Value) -> bool {
        match (self, value) {
            (Self::Null, Value::Null)
            | (Self::Boolean, Value::Bool(_))
            | (Self::Object, Value::Object(_))
            | (Self::Array, Value::Array(_))
            | (Self::Number, Value::Number(_))
            | (Self::String, Value::String(_)) => true,
            (Self::Integer, Value::Number(number)) => {
                number.is_i64()
                    || number.is_u64()
                    || number.as_f64().is_some_and(|f| f.fract() == 0.0)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    /// `true` or `{}`: accepts everything.
    Any,
    /// `false`: rejects everything.
    Never,
    Schema(Box<SchemaNode>),
}

#[derive(Debug, Clone, PartialEq, Default)]
struct SchemaNode {
    types: Vec<JsonType>,
    enumeration: Option<Vec<Value>>,
    constant: Option<Value>,
    properties: BTreeMap<String, Node>,
    required: Vec<String>,
    additional_properties: Option<Node>,
    items: Option<Node>,
    min_items: Option<u64>,
    max_items: Option<u64>,
    unique_items: bool,
    min_length: Option<u64>,
    max_length: Option<u64>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    exclusive_minimum: Option<f64>,
    exclusive_maximum: Option<f64>,
    min_properties: Option<u64>,
    max_properties: Option<u64>,
    any_of: Vec<Node>,
    one_of: Vec<Node>,
    all_of: Vec<Node>,
    not: Option<Node>,
}

/// A compiled, bounded output schema. Cheap to share across the claim and the
/// runtime loop; immutable once built.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledOutputSchema {
    root: Node,
    repair_turns: u8,
    /// Compact canonical encoding of the source schema, shown to the model
    /// in the system prompt. Serialized once here, never per turn.
    schema_json: Box<str>,
}

impl CompiledOutputSchema {
    /// Compiles and bounds a contract. Fails before any model work when the
    /// schema is too large, too deep, holds too many values, uses references
    /// or unsupported keywords, or asks for more repair turns than the
    /// runtime permits.
    pub fn compile(contract: &OutputContract) -> Result<Arc<Self>, OutputSchemaError> {
        if contract.repair_turns > MAX_OUTPUT_REPAIR_TURNS {
            return Err(OutputSchemaError::TooManyRepairTurns {
                actual: contract.repair_turns,
                limit: MAX_OUTPUT_REPAIR_TURNS,
            });
        }
        // Byte size is measured on the canonical compact encoding so the
        // bound does not depend on the caller's whitespace.
        let encoded =
            serde_json::to_vec(&contract.schema).map_err(|error| OutputSchemaError::Invalid {
                pointer: String::new(),
                message: error.to_string(),
            })?;
        if encoded.len() > MAX_OUTPUT_SCHEMA_BYTES {
            return Err(OutputSchemaError::TooLarge {
                actual: encoded.len(),
                limit: MAX_OUTPUT_SCHEMA_BYTES,
            });
        }
        let mut budget = Budget::default();
        budget.measure(&contract.schema, 0)?;
        let root = compile_node(&contract.schema, &mut String::new())?;
        let schema_json = String::from_utf8(encoded)
            .map_err(|error| OutputSchemaError::Invalid {
                pointer: String::new(),
                message: error.to_string(),
            })?
            .into_boxed_str();
        Ok(Arc::new(Self {
            root,
            repair_turns: contract.repair_turns,
            schema_json,
        }))
    }

    #[must_use]
    pub const fn repair_turns(&self) -> u8 {
        self.repair_turns
    }

    #[must_use]
    pub fn schema_json(&self) -> &str {
        &self.schema_json
    }

    /// Validates a final answer. The answer must be one JSON document,
    /// optionally wrapped in a single ```` ```json ```` fence, which models
    /// emit even when told not to.
    pub fn validate(&self, answer: &str) -> Result<Value, Vec<String>> {
        let document = strip_fence(answer.trim());
        let value: Value = match serde_json::from_str(document) {
            Ok(value) => value,
            Err(error) => {
                return Err(vec![bounded_error(format!(
                    "/: the answer is not a JSON document ({error})"
                ))]);
            }
        };
        let mut errors = Vec::new();
        validate_node(&self.root, &value, &mut String::new(), &mut errors);
        if errors.is_empty() {
            Ok(value)
        } else {
            Err(errors)
        }
    }
}

/// Counts values and depth while walking the schema. Both are bounded so a
/// hostile schema cannot make compilation or validation expensive.
#[derive(Default)]
struct Budget {
    values: usize,
}

impl Budget {
    fn measure(&mut self, value: &Value, depth: usize) -> Result<(), OutputSchemaError> {
        if depth > MAX_OUTPUT_SCHEMA_DEPTH {
            return Err(OutputSchemaError::TooDeep {
                actual: depth,
                limit: MAX_OUTPUT_SCHEMA_DEPTH,
            });
        }
        self.values += 1;
        if self.values > MAX_OUTPUT_SCHEMA_VALUES {
            return Err(OutputSchemaError::TooManyValues {
                limit: MAX_OUTPUT_SCHEMA_VALUES,
            });
        }
        match value {
            Value::Array(items) => {
                for item in items {
                    self.measure(item, depth + 1)?;
                }
            }
            Value::Object(members) => {
                for member in members.values() {
                    self.measure(member, depth + 1)?;
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
        Ok(())
    }
}

fn invalid(pointer: &str, message: impl Into<String>) -> OutputSchemaError {
    OutputSchemaError::Invalid {
        pointer: pointer.to_owned(),
        message: message.into(),
    }
}

fn compile_node(value: &Value, pointer: &mut String) -> Result<Node, OutputSchemaError> {
    let members = match value {
        Value::Bool(true) => return Ok(Node::Any),
        Value::Bool(false) => return Ok(Node::Never),
        Value::Object(members) => members,
        _ => return Err(invalid(pointer, "a schema must be an object or a boolean")),
    };
    for keyword in members.keys() {
        if keyword.starts_with('$') && !matches!(keyword.as_str(), "$schema" | "$id" | "$comment") {
            return Err(OutputSchemaError::Reference {
                pointer: pointer.clone(),
            });
        }
        if !SUPPORTED_KEYWORDS.contains(&keyword.as_str()) {
            return Err(OutputSchemaError::UnsupportedKeyword {
                pointer: pointer.clone(),
                keyword: keyword.clone(),
            });
        }
    }
    if members.is_empty() {
        return Ok(Node::Any);
    }
    let mut node = SchemaNode::default();
    if let Some(types) = members.get("type") {
        let names: Vec<&str> = match types {
            Value::String(name) => vec![name.as_str()],
            Value::Array(names) => names
                .iter()
                .map(|name| {
                    name.as_str()
                        .ok_or_else(|| invalid(pointer, "type entries must be strings"))
                })
                .collect::<Result<_, _>>()?,
            _ => {
                return Err(invalid(
                    pointer,
                    "type must be a string or an array of strings",
                ));
            }
        };
        if names.is_empty() {
            return Err(invalid(pointer, "type must name at least one type"));
        }
        for name in names {
            let parsed = JsonType::parse(name)
                .ok_or_else(|| invalid(pointer, format!("unknown type {name:?}")))?;
            if !node.types.contains(&parsed) {
                node.types.push(parsed);
            }
        }
    }
    if let Some(enumeration) = members.get("enum") {
        let Value::Array(values) = enumeration else {
            return Err(invalid(pointer, "enum must be an array"));
        };
        if values.is_empty() {
            return Err(invalid(pointer, "enum must list at least one value"));
        }
        node.enumeration = Some(values.clone());
    }
    if let Some(constant) = members.get("const") {
        node.constant = Some(constant.clone());
    }
    if let Some(properties) = members.get("properties") {
        let Value::Object(properties) = properties else {
            return Err(invalid(pointer, "properties must be an object"));
        };
        for (name, schema) in properties {
            let len = pointer.len();
            push_segment(pointer, "properties");
            push_segment(pointer, name);
            let compiled = compile_node(schema, pointer)?;
            pointer.truncate(len);
            node.properties.insert(name.clone(), compiled);
        }
    }
    if let Some(required) = members.get("required") {
        let Value::Array(names) = required else {
            return Err(invalid(pointer, "required must be an array of strings"));
        };
        for name in names {
            let Value::String(name) = name else {
                return Err(invalid(pointer, "required must be an array of strings"));
            };
            if !node.required.contains(name) {
                node.required.push(name.clone());
            }
        }
    }
    if let Some(additional) = members.get("additionalProperties") {
        let len = pointer.len();
        push_segment(pointer, "additionalProperties");
        node.additional_properties = Some(compile_node(additional, pointer)?);
        pointer.truncate(len);
    }
    if let Some(items) = members.get("items") {
        let len = pointer.len();
        push_segment(pointer, "items");
        node.items = Some(compile_node(items, pointer)?);
        pointer.truncate(len);
    }
    node.min_items = non_negative_integer(members, "minItems", pointer)?;
    node.max_items = non_negative_integer(members, "maxItems", pointer)?;
    node.min_length = non_negative_integer(members, "minLength", pointer)?;
    node.max_length = non_negative_integer(members, "maxLength", pointer)?;
    node.min_properties = non_negative_integer(members, "minProperties", pointer)?;
    node.max_properties = non_negative_integer(members, "maxProperties", pointer)?;
    node.minimum = number(members, "minimum", pointer)?;
    node.maximum = number(members, "maximum", pointer)?;
    node.exclusive_minimum = number(members, "exclusiveMinimum", pointer)?;
    node.exclusive_maximum = number(members, "exclusiveMaximum", pointer)?;
    if let Some(unique) = members.get("uniqueItems") {
        let Value::Bool(unique) = unique else {
            return Err(invalid(pointer, "uniqueItems must be a boolean"));
        };
        node.unique_items = *unique;
    }
    for (keyword, target) in [
        ("anyOf", &mut node.any_of),
        ("oneOf", &mut node.one_of),
        ("allOf", &mut node.all_of),
    ] {
        let Some(alternatives) = members.get(keyword) else {
            continue;
        };
        let Value::Array(alternatives) = alternatives else {
            return Err(invalid(
                pointer,
                format!("{keyword} must be an array of schemas"),
            ));
        };
        if alternatives.is_empty() {
            return Err(invalid(
                pointer,
                format!("{keyword} must list at least one schema"),
            ));
        }
        for (index, alternative) in alternatives.iter().enumerate() {
            let len = pointer.len();
            push_segment(pointer, keyword);
            let _ = write!(pointer, "/{index}");
            target.push(compile_node(alternative, pointer)?);
            pointer.truncate(len);
        }
    }
    if let Some(not) = members.get("not") {
        let len = pointer.len();
        push_segment(pointer, "not");
        node.not = Some(compile_node(not, pointer)?);
        pointer.truncate(len);
    }
    Ok(Node::Schema(Box::new(node)))
}

fn non_negative_integer(
    members: &serde_json::Map<String, Value>,
    keyword: &str,
    pointer: &str,
) -> Result<Option<u64>, OutputSchemaError> {
    match members.get(keyword) {
        None => Ok(None),
        Some(Value::Number(number)) => number
            .as_u64()
            .map(Some)
            .ok_or_else(|| invalid(pointer, format!("{keyword} must be a non-negative integer"))),
        Some(_) => Err(invalid(
            pointer,
            format!("{keyword} must be a non-negative integer"),
        )),
    }
}

fn number(
    members: &serde_json::Map<String, Value>,
    keyword: &str,
    pointer: &str,
) -> Result<Option<f64>, OutputSchemaError> {
    match members.get(keyword) {
        None => Ok(None),
        Some(Value::Number(number)) => number
            .as_f64()
            .map(Some)
            .ok_or_else(|| invalid(pointer, format!("{keyword} must be a number"))),
        Some(_) => Err(invalid(pointer, format!("{keyword} must be a number"))),
    }
}

/// Appends one RFC 6901 reference token.
fn push_segment(pointer: &mut String, segment: &str) {
    pointer.push('/');
    for character in segment.chars() {
        match character {
            '~' => pointer.push_str("~0"),
            '/' => pointer.push_str("~1"),
            other => pointer.push(other),
        }
    }
}

fn strip_fence(answer: &str) -> &str {
    let Some(rest) = answer.strip_prefix("```") else {
        return answer;
    };
    let Some(body) = rest.strip_suffix("```") else {
        return answer;
    };
    // Drop the info string (`json`) on the opening line.
    match body.find('\n') {
        Some(newline) => body[newline + 1..].trim(),
        None => body.trim(),
    }
}

fn bounded_error(mut message: String) -> String {
    // Errors are rendered into a notice and persisted on the run; both are
    // bounded, so a single error can never exceed the payload limit alone.
    const PER_ERROR_BYTES: usize = MAX_OUTPUT_ERROR_BYTES / MAX_OUTPUT_ERRORS;
    if message.len() > PER_ERROR_BYTES {
        let mut cut = PER_ERROR_BYTES.saturating_sub(1);
        while !message.is_char_boundary(cut) {
            cut -= 1;
        }
        message.truncate(cut);
        message.push('…');
    }
    message
}

fn report(errors: &mut Vec<String>, pointer: &str, message: impl std::fmt::Display) {
    if errors.len() >= MAX_OUTPUT_ERRORS {
        return;
    }
    let location = if pointer.is_empty() { "/" } else { pointer };
    errors.push(bounded_error(format!("{location}: {message}")));
}

/// Evaluates `node` against `value` without reporting, for combinators.
fn accepts(node: &Node, value: &Value, pointer: &mut String) -> bool {
    let mut scratch = Vec::new();
    validate_node(node, value, pointer, &mut scratch);
    scratch.is_empty()
}

fn validate_node(node: &Node, value: &Value, pointer: &mut String, errors: &mut Vec<String>) {
    let schema = match node {
        Node::Any => return,
        Node::Never => {
            report(errors, pointer, "no value is permitted here");
            return;
        }
        Node::Schema(schema) => schema,
    };
    if !schema.types.is_empty() && !schema.types.iter().any(|kind| kind.matches(value)) {
        let expected = schema
            .types
            .iter()
            .map(|kind| kind.name())
            .collect::<Vec<_>>()
            .join(" or ");
        report(
            errors,
            pointer,
            format!("expected {expected}, found {}", type_name(value)),
        );
        // Type-specific keywords cannot apply; stop here to keep the
        // feedback focused.
        return;
    }
    if let Some(constant) = &schema.constant
        && constant != value
    {
        report(errors, pointer, format!("must equal {constant}"));
    }
    if let Some(enumeration) = &schema.enumeration
        && !enumeration.contains(value)
    {
        let allowed = enumeration
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        report(errors, pointer, format!("must be one of {allowed}"));
    }
    match value {
        Value::Object(members) => {
            let count = u64::try_from(members.len()).unwrap_or(u64::MAX);
            if schema.min_properties.is_some_and(|min| count < min) {
                report(
                    errors,
                    pointer,
                    format!(
                        "must have at least {} properties",
                        schema.min_properties.unwrap_or(0)
                    ),
                );
            }
            if schema.max_properties.is_some_and(|max| count > max) {
                report(
                    errors,
                    pointer,
                    format!(
                        "must have at most {} properties",
                        schema.max_properties.unwrap_or(0)
                    ),
                );
            }
            for name in &schema.required {
                if !members.contains_key(name) {
                    report(
                        errors,
                        pointer,
                        format!("missing required property {name:?}"),
                    );
                }
            }
            for (name, member) in members {
                let len = pointer.len();
                push_segment(pointer, name);
                match schema.properties.get(name) {
                    Some(property) => validate_node(property, member, pointer, errors),
                    None => match &schema.additional_properties {
                        Some(Node::Never) => {
                            report(errors, pointer, "unexpected property");
                        }
                        Some(additional) => validate_node(additional, member, pointer, errors),
                        None => {}
                    },
                }
                pointer.truncate(len);
            }
        }
        Value::Array(items) => {
            let count = u64::try_from(items.len()).unwrap_or(u64::MAX);
            if schema.min_items.is_some_and(|min| count < min) {
                report(
                    errors,
                    pointer,
                    format!("must have at least {} items", schema.min_items.unwrap_or(0)),
                );
            }
            if schema.max_items.is_some_and(|max| count > max) {
                report(
                    errors,
                    pointer,
                    format!("must have at most {} items", schema.max_items.unwrap_or(0)),
                );
            }
            if schema.unique_items {
                for (index, item) in items.iter().enumerate() {
                    if items[..index].contains(item) {
                        let len = pointer.len();
                        let _ = write!(pointer, "/{index}");
                        report(errors, pointer, "duplicates an earlier item");
                        pointer.truncate(len);
                        break;
                    }
                }
            }
            if let Some(item_schema) = &schema.items {
                for (index, item) in items.iter().enumerate() {
                    let len = pointer.len();
                    let _ = write!(pointer, "/{index}");
                    validate_node(item_schema, item, pointer, errors);
                    pointer.truncate(len);
                }
            }
        }
        Value::String(text) => {
            let length = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
            if schema.min_length.is_some_and(|min| length < min) {
                report(
                    errors,
                    pointer,
                    format!(
                        "must be at least {} characters",
                        schema.min_length.unwrap_or(0)
                    ),
                );
            }
            if schema.max_length.is_some_and(|max| length > max) {
                report(
                    errors,
                    pointer,
                    format!(
                        "must be at most {} characters",
                        schema.max_length.unwrap_or(0)
                    ),
                );
            }
        }
        Value::Number(number) => {
            let Some(actual) = number.as_f64() else {
                return;
            };
            if schema.minimum.is_some_and(|min| actual < min) {
                report(
                    errors,
                    pointer,
                    format!("must be at least {}", schema.minimum.unwrap_or(0.0)),
                );
            }
            if schema.maximum.is_some_and(|max| actual > max) {
                report(
                    errors,
                    pointer,
                    format!("must be at most {}", schema.maximum.unwrap_or(0.0)),
                );
            }
            if schema.exclusive_minimum.is_some_and(|min| actual <= min) {
                report(
                    errors,
                    pointer,
                    format!(
                        "must be greater than {}",
                        schema.exclusive_minimum.unwrap_or(0.0)
                    ),
                );
            }
            if schema.exclusive_maximum.is_some_and(|max| actual >= max) {
                report(
                    errors,
                    pointer,
                    format!(
                        "must be less than {}",
                        schema.exclusive_maximum.unwrap_or(0.0)
                    ),
                );
            }
        }
        Value::Null | Value::Bool(_) => {}
    }
    if !schema.any_of.is_empty() && !schema.any_of.iter().any(|alt| accepts(alt, value, pointer)) {
        report(errors, pointer, "matches none of the anyOf alternatives");
    }
    if !schema.one_of.is_empty() {
        let matching = schema
            .one_of
            .iter()
            .filter(|alt| accepts(alt, value, pointer))
            .count();
        if matching != 1 {
            report(
                errors,
                pointer,
                format!("matches {matching} oneOf alternatives; exactly one is required"),
            );
        }
    }
    for (index, alternative) in schema.all_of.iter().enumerate() {
        if !accepts(alternative, value, pointer) {
            report(errors, pointer, format!("does not satisfy allOf/{index}"));
        }
    }
    if let Some(not) = &schema.not
        && accepts(not, value, pointer)
    {
        report(errors, pointer, "matches the forbidden schema");
    }
}

const fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

pub(crate) const OUTPUT_REPAIR_NOTICE: &str = "[QQ runtime notice; not a user instruction]\nYour \
final answer must be exactly one JSON document that satisfies the required output schema. The \
previous answer failed validation with the errors below. Reply with only the corrected JSON \
document, with no prose before or after it.";

/// The system-prompt paragraph a run with an output contract carries so the
/// model knows the shape before its first turn.
pub(crate) const OUTPUT_CONTRACT_SYSTEM_NOTICE: &str = "## Output contract\n\nYour final answer \
(the message in which you call no tools) must be exactly one JSON document, with no prose before \
or after it, that satisfies this JSON Schema:\n\n```json\n";

/// Renders the repair notice for one failed validation, bounded to
/// `MAX_OUTPUT_ERROR_BYTES` of error detail.
pub(crate) fn repair_notice(errors: &[String]) -> String {
    let mut notice = String::from(OUTPUT_REPAIR_NOTICE);
    let mut detail = 0_usize;
    for error in errors {
        let line = 3 + error.len();
        if detail.saturating_add(line) > MAX_OUTPUT_ERROR_BYTES {
            notice.push_str("\n- (further errors omitted)");
            break;
        }
        detail += line;
        notice.push_str("\n- ");
        notice.push_str(error);
    }
    notice
}

/// Bounds a validation-failure list for the durable `FinalOutput::Invalid`.
pub(crate) fn bounded_errors(mut errors: Vec<String>) -> Vec<String> {
    let mut total = 0_usize;
    errors.retain(|error| {
        if total.saturating_add(error.len()) > MAX_OUTPUT_ERROR_BYTES {
            return false;
        }
        total += error.len();
        true
    });
    errors
}

/// Builds the durable verdict from a validation result.
pub(crate) fn final_output(result: Result<Value, Vec<String>>, repair_turns: u8) -> FinalOutput {
    match result {
        Ok(value) => FinalOutput::Valid {
            value,
            repair_turns,
        },
        Err(errors) => FinalOutput::Invalid {
            errors: bounded_errors(errors),
            repair_turns,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn compile(schema: Value) -> Result<Arc<CompiledOutputSchema>, OutputSchemaError> {
        CompiledOutputSchema::compile(&OutputContract {
            schema,
            repair_turns: 2,
        })
    }

    #[test]
    fn an_object_schema_accepts_a_conforming_answer_and_rejects_each_violation() {
        let schema = compile(json!({
            "type": "object",
            "properties": {
                "summary": {"type": "string", "minLength": 1},
                "score": {"type": "integer", "minimum": 0, "maximum": 10},
                "tags": {"type": "array", "items": {"type": "string"}, "maxItems": 3, "uniqueItems": true},
                "kind": {"enum": ["bug", "feature"]}
            },
            "required": ["summary", "score"],
            "additionalProperties": false
        }))
        .unwrap();
        let valid = schema
            .validate(r#"{"summary":"ok","score":7,"tags":["a","b"],"kind":"bug"}"#)
            .unwrap();
        assert_eq!(valid["score"], 7);

        let errors = schema
            .validate(r#"{"score":11,"tags":["a","a","b","c"],"kind":"other","extra":1}"#)
            .unwrap_err();
        assert!(
            errors
                .iter()
                .any(|e| e == "/: missing required property \"summary\""),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e == "/score: must be at most 10"),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e == "/tags: must have at most 3 items"),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e == "/tags/1: duplicates an earlier item"),
            "{errors:?}"
        );
        assert!(
            errors
                .iter()
                .any(|e| e.starts_with("/kind: must be one of")),
            "{errors:?}"
        );
        assert!(
            errors.iter().any(|e| e == "/extra: unexpected property"),
            "{errors:?}"
        );
    }

    #[test]
    fn type_mismatches_report_expected_and_actual_types() {
        let schema = compile(json!({"type": ["integer", "null"]})).unwrap();
        assert_eq!(
            schema.validate("1.5").unwrap_err(),
            vec!["/: expected integer or null, found number".to_owned()]
        );
        assert!(schema.validate("null").is_ok());
        assert!(schema.validate("3.0").is_ok());
    }

    #[test]
    fn a_fenced_json_answer_is_unwrapped_and_prose_is_rejected() {
        let schema = compile(json!({"type": "object"})).unwrap();
        assert!(schema.validate("```json\n{\"a\": 1}\n```").is_ok());
        assert!(schema.validate("```\n{}\n```").is_ok());
        let errors = schema.validate("Here is the result: {}").unwrap_err();
        assert_eq!(errors.len(), 1);
        assert!(
            errors[0].starts_with("/: the answer is not a JSON document"),
            "{errors:?}"
        );
    }

    #[test]
    fn combinators_and_const_validate() {
        let schema = compile(json!({
            "oneOf": [{"type": "string"}, {"type": "number"}],
            "not": {"const": "forbidden"}
        }))
        .unwrap();
        assert!(schema.validate("\"ok\"").is_ok());
        assert!(schema.validate("1").is_ok());
        assert_eq!(
            schema.validate("\"forbidden\"").unwrap_err(),
            vec!["/: matches the forbidden schema".to_owned()]
        );
        assert_eq!(
            schema.validate("true").unwrap_err(),
            vec!["/: matches 0 oneOf alternatives; exactly one is required".to_owned()]
        );
        let all = compile(json!({"allOf": [{"type": "integer"}, {"minimum": 5}]})).unwrap();
        assert_eq!(
            all.validate("3").unwrap_err(),
            vec!["/: does not satisfy allOf/1".to_owned()]
        );
        let any = compile(json!({"anyOf": [{"type": "integer"}, {"type": "null"}]})).unwrap();
        assert_eq!(
            any.validate("\"x\"").unwrap_err(),
            vec!["/: matches none of the anyOf alternatives".to_owned()]
        );
    }

    #[test]
    fn references_and_unsupported_keywords_fail_compilation_with_their_pointer() {
        assert_eq!(
            compile(json!({"properties": {"a": {"$ref": "#/x"}}})).unwrap_err(),
            OutputSchemaError::Reference {
                pointer: "/properties/a".to_owned()
            }
        );
        assert_eq!(
            compile(json!({"$defs": {"a": {}}})).unwrap_err(),
            OutputSchemaError::Reference {
                pointer: String::new()
            }
        );
        assert_eq!(
            compile(json!({"type": "string", "pattern": "^a"})).unwrap_err(),
            OutputSchemaError::UnsupportedKeyword {
                pointer: String::new(),
                keyword: "pattern".to_owned()
            }
        );
        assert_eq!(
            compile(json!({"items": {"type": "bogus"}})).unwrap_err(),
            OutputSchemaError::Invalid {
                pointer: "/items".to_owned(),
                message: "unknown type \"bogus\"".to_owned()
            }
        );
        assert!(matches!(
            compile(json!(7)).unwrap_err(),
            OutputSchemaError::Invalid { .. }
        ));
    }

    #[test]
    fn schema_bounds_are_enforced_at_compilation() {
        let big = json!({"description": "x".repeat(MAX_OUTPUT_SCHEMA_BYTES)});
        assert!(matches!(
            compile(big).unwrap_err(),
            OutputSchemaError::TooLarge { .. }
        ));

        let mut deep = json!({"type": "array"});
        for _ in 0..(MAX_OUTPUT_SCHEMA_DEPTH + 1) {
            deep = json!({"type": "array", "items": deep});
        }
        assert!(matches!(
            compile(deep).unwrap_err(),
            OutputSchemaError::TooDeep { .. }
        ));

        let many: Vec<Value> = (0..MAX_OUTPUT_SCHEMA_VALUES).map(|i| json!(i)).collect();
        assert_eq!(
            compile(json!({"enum": many})).unwrap_err(),
            OutputSchemaError::TooManyValues {
                limit: MAX_OUTPUT_SCHEMA_VALUES
            }
        );

        assert_eq!(
            CompiledOutputSchema::compile(&OutputContract {
                schema: json!({}),
                repair_turns: MAX_OUTPUT_REPAIR_TURNS + 1,
            })
            .unwrap_err(),
            OutputSchemaError::TooManyRepairTurns {
                actual: 9,
                limit: 8
            }
        );
        assert_eq!(
            CompiledOutputSchema::compile(&OutputContract {
                schema: json!(true),
                repair_turns: 0,
            })
            .unwrap()
            .repair_turns(),
            0
        );
    }

    #[test]
    fn error_lists_and_repair_notices_stay_within_the_payload_bound() {
        let schema = compile(json!({"type": "object", "additionalProperties": false})).unwrap();
        let members = (0..100)
            .map(|i| format!("\"{}\": 1", "k".repeat(600) + &i.to_string()))
            .collect::<Vec<_>>()
            .join(",");
        let errors = schema.validate(&format!("{{{members}}}")).unwrap_err();
        assert_eq!(errors.len(), MAX_OUTPUT_ERRORS);
        let notice = repair_notice(&errors);
        assert!(notice.len() <= OUTPUT_REPAIR_NOTICE.len() + MAX_OUTPUT_ERROR_BYTES + 32);
        let durable = bounded_errors(errors);
        assert!(durable.iter().map(String::len).sum::<usize>() <= MAX_OUTPUT_ERROR_BYTES);
        assert!(!durable.is_empty());
    }

    /// Enabled-path cost, printed for the ledger: `cargo test -p qq-core
    /// --release output::tests::enabled_path_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "measurement, not a check"]
    fn enabled_path_cost() {
        let mut properties = serde_json::Map::new();
        for i in 0..64 {
            properties.insert(
                format!("field_{i}"),
                json!({"type": ["string", "null"], "maxLength": 256}),
            );
        }
        let schema = json!({
            "type": "object",
            "properties": properties,
            "required": ["field_0", "field_1"],
            "additionalProperties": false
        });
        let contract = OutputContract {
            schema,
            repair_turns: 2,
        };
        let mut answer = serde_json::Map::new();
        for i in 0..64 {
            answer.insert(format!("field_{i}"), json!("x".repeat(100)));
        }
        let answer = serde_json::to_string(&Value::Object(answer)).unwrap();
        let started = std::time::Instant::now();
        let mut compiled = CompiledOutputSchema::compile(&contract).unwrap();
        for _ in 1..1000 {
            compiled = CompiledOutputSchema::compile(&contract).unwrap();
        }
        let compile_ns = started.elapsed().as_nanos() / 1000;
        let started = std::time::Instant::now();
        for _ in 0..1000 {
            assert!(compiled.validate(&answer).is_ok());
        }
        let validate_ns = started.elapsed().as_nanos() / 1000;
        println!(
            "output contract (64 properties, {} B schema, {} B answer): compile {compile_ns} ns, validate {validate_ns} ns",
            compiled.schema_json().len(),
            answer.len()
        );
    }

    #[test]
    fn a_single_oversized_error_is_truncated_on_a_char_boundary() {
        let error = bounded_error("é".repeat(MAX_OUTPUT_ERROR_BYTES));
        assert!(error.len() <= MAX_OUTPUT_ERROR_BYTES / MAX_OUTPUT_ERRORS + 3);
        assert!(error.ends_with('…'));
    }
}
