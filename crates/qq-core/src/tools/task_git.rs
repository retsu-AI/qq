//! Arguments for task-scoped Git operations. Targets and authority are server-owned.
use qq_provider::ToolSpec;
use serde::Deserialize;

pub(crate) const MAX_MESSAGE_BYTES: usize = 16 * 1024;
pub(crate) const MAX_TITLE_BYTES: usize = 256;
pub(crate) const MAX_BODY_BYTES: usize = 64 * 1024;

pub(crate) fn specs() -> [ToolSpec; 3] {
    [
        ToolSpec::new(
            "task_commit",
            "Commit the task's admitted changes with a message. QQ selects the files, private task repository and branch; this does not run shell commands or hooks.",
            serde_json::json!({
                "type":"object", "additionalProperties":false, "required":["message"],
                "properties":{"message":{"type":"string","minLength":1,"maxLength":MAX_MESSAGE_BYTES,
                    "description":"Nonblank commit message, at most 16384 UTF-8 bytes; no NUL characters."}}
            }),
        ),
        ToolSpec::new(
            "task_push",
            "Push the task branch to its server-authorized destination. QQ supplies the repository, branch and scoped credentials; no destination or Git options are accepted.",
            serde_json::json!({"type":"object","additionalProperties":false,"properties":{}}),
        ),
        ToolSpec::new(
            "task_pr",
            "Create or update the task branch's pull request with a title and body. QQ supplies the authorized repository and branches; no URLs or credentials are accepted.",
            serde_json::json!({
                "type":"object", "additionalProperties":false, "required":["title","body"],
                "properties":{
                    "title":{"type":"string","minLength":1,"maxLength":MAX_TITLE_BYTES,
                        "description":"Nonblank single-line title, at most 256 UTF-8 bytes; no control characters."},
                    "body":{"type":"string","maxLength":MAX_BODY_BYTES,
                        "description":"Pull request body, at most 65536 UTF-8 bytes; no NUL characters."}
                }
            }),
        ),
    ]
}

pub(crate) fn is_tool(name: &str) -> bool {
    matches!(name, "task_commit" | "task_push" | "task_pr")
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TaskGitRequest {
    Commit { message: String },
    Push,
    PullRequest { title: String, body: String },
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TaskGitArgumentsError {
    #[error("invalid task Git arguments: {0}")]
    Arguments(#[from] serde_json::Error),
    #[error("invalid task Git {0}")]
    Invalid(&'static str),
}

pub(crate) fn parse(name: &str, arguments: &str) -> Result<TaskGitRequest, TaskGitArgumentsError> {
    if !arguments.trim_start().starts_with('{') {
        return Err(TaskGitArgumentsError::Invalid("argument object"));
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Commit {
        message: String,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Push {}
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct PullRequest {
        title: String,
        body: String,
    }
    match name {
        "task_commit" => {
            let Commit { message } = serde_json::from_str(arguments)?;
            if !valid_text(&message, MAX_MESSAGE_BYTES) || message.trim().is_empty() {
                return Err(TaskGitArgumentsError::Invalid("commit message"));
            }
            Ok(TaskGitRequest::Commit { message })
        }
        "task_push" => {
            let Push {} = serde_json::from_str(arguments)?;
            Ok(TaskGitRequest::Push)
        }
        "task_pr" => {
            let PullRequest { title, body } = serde_json::from_str(arguments)?;
            if !valid_text(&title, MAX_TITLE_BYTES)
                || title.trim().is_empty()
                || title.chars().any(char::is_control)
            {
                return Err(TaskGitArgumentsError::Invalid("pull request title"));
            }
            if !valid_text(&body, MAX_BODY_BYTES) {
                return Err(TaskGitArgumentsError::Invalid("pull request body"));
            }
            Ok(TaskGitRequest::PullRequest { title, body })
        }
        _ => Err(TaskGitArgumentsError::Invalid("tool name")),
    }
}

fn valid_text(value: &str, limit: usize) -> bool {
    value.len() <= limit && !value.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_and_parsers_accept_only_task_operation_fields() {
        assert_eq!(parse("task_push", "{}").unwrap(), TaskGitRequest::Push);
        assert_eq!(
            parse(
                "task_commit",
                r#"{"message":"Fix parser\n\nKeep quoted `$(cmd)` text."}"#
            )
            .unwrap(),
            TaskGitRequest::Commit {
                message: "Fix parser\n\nKeep quoted `$(cmd)` text.".into()
            }
        );
        assert_eq!(
            parse("task_pr", r#"{"title":"Fix parser","body":""}"#).unwrap(),
            TaskGitRequest::PullRequest {
                title: "Fix parser".into(),
                body: String::new()
            }
        );
        for spec in specs() {
            let schema: serde_json::Value =
                serde_json::from_str(spec.input_schema().get()).unwrap();
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["additionalProperties"], false);
            for name in [
                "url",
                "remote",
                "repository",
                "branch",
                "ref",
                "options",
                "credentials",
                "token",
                "paths",
            ] {
                let mut args = match spec.name() {
                    "task_commit" => serde_json::json!({"message":"fix"}),
                    "task_pr" => serde_json::json!({"title":"fix","body":"details"}),
                    _ => serde_json::json!({}),
                };
                args[name] = serde_json::json!("untrusted");
                assert!(
                    parse(spec.name(), &args.to_string()).is_err(),
                    "{} accepted {name}",
                    spec.name()
                );
            }
        }
        for (name, args) in [
            ("task_commit", "{}"),
            ("task_pr", r#"{"title":"fix"}"#),
            ("task_push", "[]"),
            ("task_push", "null"),
            ("task_commit", r#"{"message":"a","message":"b"}"#),
            ("task_pr", r#"{"title":42,"body":"text"}"#),
            ("unknown", "{}"),
        ] {
            assert!(parse(name, args).is_err(), "{name}: {args}");
        }
    }

    #[test]
    fn text_limits_count_utf8_bytes_and_reject_control_inputs() {
        let commit = |message: &str| {
            parse(
                "task_commit",
                &serde_json::json!({"message":message}).to_string(),
            )
        };
        let pull_request = |title: &str, body: &str| {
            parse(
                "task_pr",
                &serde_json::json!({"title":title,"body":body}).to_string(),
            )
        };
        assert!(commit(&"x".repeat(MAX_MESSAGE_BYTES)).is_ok());
        assert!(commit(&"x".repeat(MAX_MESSAGE_BYTES + 1)).is_err());
        assert!(commit(&"é".repeat(MAX_MESSAGE_BYTES / 2 + 1)).is_err());
        for message in ["", " \n\t", "embedded\0nul"] {
            assert!(commit(message).is_err());
        }
        assert!(pull_request(&"x".repeat(MAX_TITLE_BYTES), &"x".repeat(MAX_BODY_BYTES)).is_ok());
        assert!(pull_request(&"é".repeat(MAX_TITLE_BYTES / 2 + 1), "").is_err());
        assert!(pull_request("fix", &"é".repeat(MAX_BODY_BYTES / 2 + 1)).is_err());
        for title in [
            "",
            "   ",
            "line\nbreak",
            "line\rbreak",
            "tab\ttitle",
            "nul\0title",
        ] {
            assert!(pull_request(title, "").is_err());
        }
        assert!(pull_request("fix", "embedded\0nul").is_err());
    }
}
