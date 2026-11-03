use std::sync::Arc;

use qq_provider::ToolSpec;
use qq_script::{FailureKind, Language, ScriptFailure, ToolFailure, ToolSignature};
use serde_json::json;

use crate::{catalog, tools};

pub(crate) const RUN_CODE_TOOL: &str = "run_code";
pub(crate) const SCRIPT_CHILD_CALL_PREFIX: &str = "qq-script:";
const SCRIPT_CHILD_ORDINAL_BASE: u16 = 1024;
const MAX_SCRIPT_CALLS: u16 = 256;
const MAX_DECLARED_EXTERNAL_TOOLS: usize = 24;
const EXCLUDED_HOSTS: [catalog::ToolHost; 3] = [
    catalog::ToolHost::RunCode,
    catalog::ToolHost::SelectTools,
    catalog::ToolHost::UpdateGoal,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScriptTool {
    pub(crate) script_name: String,
    pub(crate) tool_name: String,
    pub(crate) signature: ToolSignature,
}

#[derive(Debug, Clone)]
pub(crate) struct CodeSurface {
    pub(crate) language: Language,
    pub(crate) tools: Arc<[ScriptTool]>,
    pub(crate) spec: ToolSpec,
}

impl CodeSurface {
    pub(crate) fn new(
        language: Language,
        catalog: &catalog::ToolCatalog,
        filter: &catalog::StaticFilter,
    ) -> Self {
        let mut external = 0_usize;
        let mut tools = Vec::new();
        for entry in catalog.entries() {
            if EXCLUDED_HOSTS.contains(&entry.host) || !filter.allows(entry) {
                continue;
            }
            let tool_name = entry.spec.name().to_owned();
            let script_name = script_name(&tool_name);
            let schema =
                serde_json::from_str(entry.spec.input_schema().get()).unwrap_or_else(|_| json!({}));
            let mut signature =
                ToolSignature::from_schema(&script_name, entry.spec.description(), &schema);
            if matches!(entry.host, catalog::ToolHost::External { .. }) {
                external += 1;
                if external > MAX_DECLARED_EXTERNAL_TOOLS {
                    signature.description.clear();
                    for parameter in &mut signature.parameters {
                        parameter.description = None;
                    }
                }
            }
            tools.push(ScriptTool {
                script_name,
                tool_name,
                signature,
            });
        }
        let signatures: Vec<ToolSignature> =
            tools.iter().map(|tool| tool.signature.clone()).collect();
        let spec = run_code_spec(language, &signatures);
        Self {
            language,
            tools: tools.into(),
            spec,
        }
    }

    pub(crate) fn tool(&self, script_name: &str) -> Option<&ScriptTool> {
        self.tools
            .iter()
            .find(|tool| tool.script_name == script_name)
    }

    pub(crate) fn program(&self, source: String) -> qq_script::Program {
        qq_script::Program {
            language: self.language,
            source,
            tools: self
                .tools
                .iter()
                .map(|tool| tool.signature.clone())
                .collect(),
            limits: qq_script::Limits {
                max_calls: MAX_SCRIPT_CALLS,
                ..qq_script::Limits::default()
            },
        }
    }
}

fn script_name(tool_name: &str) -> String {
    match tool_name {
        "exec" => "run_program".to_owned(),
        name => name.replace('-', "_"),
    }
}

fn run_code_spec(language: Language, signatures: &[ToolSignature]) -> ToolSpec {
    let declarations = qq_script::declarations(language, signatures);
    let description = match language {
        Language::Python => format!(
            "Run a Python script in a sandbox. The script calls QQ's tools as async functions and must await them; \
             independent calls go together in asyncio.gather and run concurrently. Each call returns the tool's \
             text result. A denied call raises PermissionError, a missing path FileNotFoundError, bad arguments \
             ValueError or TypeError, any other failure RuntimeError. Only print() output and the value of the \
             last expression come back, so print what you need and keep it short. Variables do not persist \
             between run_code calls. There is no filesystem, network or subprocess access except through these \
             functions; exec is named run_program here. Most of the standard library is unavailable; asyncio, \
             json, re, math and datetime are present.\n\n```python\n{declarations}```"
        ),
        Language::JavaScript => format!(
            "Run a JavaScript script in a sandbox. Call QQ's tools as async functions taking one object and \
             await them; use Promise.all for independent calls. Each call resolves to the tool's text result \
             and rejects with an Error when the tool fails. Only console.log output and the value of the last \
             expression come back.\n\n```ts\n{declarations}```"
        ),
    };
    ToolSpec::new(
        RUN_CODE_TOOL,
        description,
        json!({
            "type": "object",
            "properties": {
                "code": { "type": "string", "description": "The script source." }
            },
            "required": ["code"],
            "additionalProperties": false
        }),
    )
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunCodeArgs {
    pub(crate) code: String,
}

pub(crate) fn child_ordinal(parent_ordinal: u16, seq: u16) -> u16 {
    SCRIPT_CHILD_ORDINAL_BASE
        .saturating_add(
            parent_ordinal
                .saturating_sub(1)
                .saturating_mul(MAX_SCRIPT_CALLS),
        )
        .saturating_add(seq)
}

pub(crate) fn child_provider_id(parent_ordinal: u16, seq: u16) -> String {
    format!("{SCRIPT_CHILD_CALL_PREFIX}{parent_ordinal}:{seq}")
}

pub(crate) fn settle(output: &tools::ToolOutput) -> Result<serde_json::Value, ToolFailure> {
    let ran_to_exit = matches!(
        output.native_outcome,
        Some(tools::dispatch::CommandOutcome::Exited { .. })
    );
    if !output.is_error || ran_to_exit {
        return Ok(serde_json::Value::String(output.model_text.clone()));
    }
    Err(failure(&output.model_text, output.disposition))
}

pub(crate) fn unknown(name: &str) -> ToolFailure {
    ToolFailure {
        kind: FailureKind::InvalidArguments,
        message: format!("{name} is not available in this run"),
    }
}

pub(crate) fn interrupted() -> qq_script::Completion {
    qq_script::Completion {
        printed: qq_script::Printed::default(),
        value: None,
        failure: Some(ScriptFailure::Raised(
            crate::INTERRUPTED_TOOL_RESULT.to_owned(),
        )),
        unawaited: Vec::new(),
    }
}

pub(crate) fn denied(message: &str) -> ToolFailure {
    ToolFailure {
        kind: FailureKind::Permission,
        message: message.to_owned(),
    }
}

fn failure(text: &str, disposition: tools::ResultDisposition) -> ToolFailure {
    let kind = if text.starts_with("permission_denied") {
        FailureKind::Permission
    } else if text.starts_with("path_not_found") || text.starts_with("not_found") {
        FailureKind::NotFound
    } else if text.contains("timed out") {
        FailureKind::Timeout
    } else if disposition == tools::ResultDisposition::Refused {
        FailureKind::InvalidArguments
    } else {
        FailureKind::Failed
    };
    ToolFailure {
        kind,
        message: text.to_owned(),
    }
}

pub(crate) fn render(completion: &qq_script::Completion) -> (String, bool) {
    let mut text = completion.printed.render();
    let mut push = |line: &str| {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(line);
    };
    if let Some(value) = &completion.value {
        let rendered = match value {
            serde_json::Value::String(value) => value.clone(),
            value => value.to_string(),
        };
        push(&format!("[result] {rendered}"));
    }
    if !completion.unawaited.is_empty() {
        push(&format!(
            "[warning] never awaited, so never ran: {}",
            completion.unawaited.join(", ")
        ));
    }
    let is_error = completion.failure.is_some();
    match &completion.failure {
        Some(ScriptFailure::Compile(message)) => {
            push(&format!("[script did not start]\n{message}"));
        }
        Some(ScriptFailure::Raised(message)) => push(&format!("[script raised]\n{message}")),
        Some(ScriptFailure::Limit(message)) => {
            push(&format!("[script stopped at a limit] {message}"));
        }
        None => {}
    }
    if text.is_empty() {
        text.push_str("[no output; print() what you need]");
    }
    (text, is_error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_ordinals_never_collide_with_model_calls_or_each_other() {
        assert_eq!(child_ordinal(1, 0), 1024);
        assert_eq!(child_ordinal(1, 255), 1279);
        assert_eq!(child_ordinal(2, 0), 1280);
        assert!(child_ordinal(64, 255) < u16::MAX);
    }

    #[test]
    fn exec_is_renamed_and_dashes_become_underscores() {
        assert_eq!(script_name("exec"), "run_program");
        assert_eq!(script_name("mcp__a-b__c"), "mcp__a_b__c");
        assert_eq!(script_name("read_file"), "read_file");
    }

    #[test]
    fn completion_renders_output_value_and_failure() {
        let completion = qq_script::Completion {
            printed: qq_script::Printed {
                head: "hi\n".to_owned(),
                ..qq_script::Printed::default()
            },
            value: Some(json!({"n": 1})),
            failure: Some(ScriptFailure::Raised("ValueError: x".to_owned())),
            unawaited: vec!["search".to_owned()],
        };
        assert_eq!(
            render(&completion),
            (
                "hi\n[result] {\"n\":1}\n[warning] never awaited, so never ran: search\n[script raised]\nValueError: x"
                    .to_owned(),
                true
            )
        );
    }

    #[test]
    fn empty_completion_tells_the_model_to_print() {
        let completion = qq_script::Completion {
            printed: qq_script::Printed::default(),
            value: None,
            failure: None,
            unawaited: Vec::new(),
        };
        assert_eq!(
            render(&completion),
            ("[no output; print() what you need]".to_owned(), false)
        );
    }
}
