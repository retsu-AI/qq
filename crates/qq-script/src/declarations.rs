use serde_json::Value;

use crate::Language;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterSpec {
    pub name: String,
    pub type_hint: String,
    pub required: bool,
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSignature {
    pub name: String,
    pub description: String,
    pub parameters: Vec<ParameterSpec>,
    pub accepts_any: bool,
}

impl ToolSignature {
    #[must_use]
    pub fn from_schema(name: &str, description: &str, schema: &Value) -> Self {
        let properties = schema.get("properties").and_then(Value::as_object);
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut parameters: Vec<ParameterSpec> = properties
            .into_iter()
            .flatten()
            .filter(|(name, _)| is_identifier(name))
            .map(|(name, property)| ParameterSpec {
                name: name.clone(),
                type_hint: python_type(property),
                required: required.contains(&name.as_str()),
                description: property
                    .get("description")
                    .and_then(Value::as_str)
                    .map(first_sentence),
            })
            .collect();
        parameters.sort_by_key(|parameter| !parameter.required);
        let accepts_any = properties.is_none()
            || properties
                .is_some_and(|properties| properties.keys().any(|key| !is_identifier(key)))
            || schema
                .get("additionalProperties")
                .is_some_and(|value| value != &Value::Bool(false));
        Self {
            name: name.to_owned(),
            description: description.to_owned(),
            parameters,
            accepts_any,
        }
    }
}

#[must_use]
pub fn declarations(language: Language, tools: &[ToolSignature]) -> String {
    match language {
        Language::Python => python_declarations(tools),
        Language::JavaScript => javascript_declarations(tools),
    }
}

fn python_declarations(tools: &[ToolSignature]) -> String {
    let mut text = String::new();
    for tool in tools {
        let mut parameters: Vec<String> = Vec::with_capacity(tool.parameters.len() + 1);
        let mut keyword_only = false;
        for parameter in &tool.parameters {
            if !parameter.required && !keyword_only {
                keyword_only = true;
                parameters.push("*".to_owned());
            }
            let default = if parameter.required { "" } else { " = None" };
            parameters.push(format!(
                "{}: {}{default}",
                parameter.name, parameter.type_hint
            ));
        }
        if tool.accepts_any {
            parameters.push("**kwargs".to_owned());
        }
        text.push_str(&format!(
            "async def {}({}) -> dict:\n",
            tool.name,
            parameters.join(", ")
        ));
        let summary = first_sentence(&tool.description);
        let documented: Vec<&ParameterSpec> = tool
            .parameters
            .iter()
            .filter(|parameter| parameter.description.is_some())
            .collect();
        if summary.is_empty() && documented.is_empty() {
            text.push_str("    ...\n\n");
            continue;
        }
        text.push_str("    \"\"\"");
        text.push_str(&summary.replace("\"\"\"", "'''"));
        for parameter in documented {
            text.push_str(&format!(
                "\n    {}: {}",
                parameter.name,
                parameter
                    .description
                    .as_deref()
                    .unwrap_or_default()
                    .replace("\"\"\"", "'''")
            ));
        }
        text.push_str("\"\"\"\n\n");
    }
    text
}

fn javascript_declarations(tools: &[ToolSignature]) -> String {
    let mut text = String::new();
    for tool in tools {
        let fields: Vec<String> = tool
            .parameters
            .iter()
            .map(|parameter| {
                let optional = if parameter.required { "" } else { "?" };
                format!(
                    "{}{optional}: {}",
                    parameter.name,
                    js_type(&parameter.type_hint)
                )
            })
            .collect();
        let summary = first_sentence(&tool.description);
        if !summary.is_empty() {
            text.push_str(&format!("/** {} */\n", summary.replace("*/", "* /")));
        }
        text.push_str(&format!(
            "declare function {}(args: {{ {} }}): Promise<any>;\n",
            tool.name,
            fields.join("; ")
        ));
    }
    text
}

fn js_type(python: &str) -> &'static str {
    match python {
        "str" => "string",
        "int" | "float" => "number",
        "bool" => "boolean",
        "list" => "any[]",
        _ => "any",
    }
}

fn python_type(property: &Value) -> String {
    let kind = match property.get("type") {
        Some(Value::String(kind)) => kind.as_str(),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .find(|kind| *kind != "null")
            .unwrap_or("any"),
        _ => "any",
    };
    match kind {
        "string" => "str",
        "integer" => "int",
        "number" => "float",
        "boolean" => "bool",
        "array" => "list",
        "object" => "dict",
        _ => "object",
    }
    .to_owned()
}

fn first_sentence(text: &str) -> String {
    let text = text.trim();
    let end = text
        .char_indices()
        .find(|&(index, character)| {
            character == '\n'
                || (character == '.'
                    && text[index + 1..]
                        .chars()
                        .next()
                        .is_none_or(char::is_whitespace))
        })
        .map_or(text.len(), |(index, character)| {
            index + usize::from(character == '.')
        });
    let sentence = &text[..end];
    if sentence.len() <= 240 {
        return sentence.to_owned();
    }
    let mut cut = 237;
    while !sentence.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &sentence[..cut])
}

pub(crate) fn is_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric())
        && !matches!(
            name,
            "False"
                | "None"
                | "True"
                | "and"
                | "as"
                | "assert"
                | "async"
                | "await"
                | "break"
                | "class"
                | "continue"
                | "def"
                | "del"
                | "elif"
                | "else"
                | "except"
                | "finally"
                | "for"
                | "from"
                | "global"
                | "if"
                | "import"
                | "in"
                | "is"
                | "lambda"
                | "nonlocal"
                | "not"
                | "or"
                | "pass"
                | "raise"
                | "return"
                | "try"
                | "while"
                | "with"
                | "yield"
        )
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn read_file() -> ToolSignature {
        ToolSignature::from_schema(
            "read_file",
            "Read a workspace file. Returns numbered lines.",
            &json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Workspace-relative path."},
                    "start_line": {"type": ["integer", "null"]}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        )
    }

    #[test]
    fn python_stub_puts_optional_parameters_after_a_star() {
        assert_eq!(
            declarations(Language::Python, &[read_file()]),
            "async def read_file(path: str, *, start_line: int = None) -> dict:\n    \"\"\"Read a workspace file.\n    path: Workspace-relative path.\"\"\"\n\n"
        );
    }

    #[test]
    fn open_schema_accepts_keyword_arguments() {
        let signature = ToolSignature::from_schema("mcp__x__y", "", &json!({"type": "object"}));
        assert!(signature.accepts_any);
        assert_eq!(
            declarations(Language::Python, &[signature]),
            "async def mcp__x__y(**kwargs) -> dict:\n    ...\n\n"
        );
    }

    #[test]
    fn javascript_stub_takes_one_object() {
        assert_eq!(
            declarations(Language::JavaScript, &[read_file()]),
            "/** Read a workspace file. */\ndeclare function read_file(args: { path: string; start_line?: number }): Promise<any>;\n"
        );
    }

    #[test]
    fn keywords_and_bad_names_are_not_identifiers() {
        assert!(is_identifier("read_file"));
        assert!(!is_identifier("class"));
        assert!(!is_identifier("a-b"));
        assert!(!is_identifier("1x"));
    }
}
