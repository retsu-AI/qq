use std::sync::OnceLock;

use qq_protocol::{ChildAuthority, DelegationRole, DelegationRoster};
use qq_provider::ToolSpec;
use serde::Deserialize;
use serde_json::json;

use super::{
    ask::{MAX_OPTION_BYTES, MAX_OPTIONS, MAX_QUESTION_BYTES, MAX_QUESTIONS, MIN_OPTIONS},
    edit::MAX_EDITS,
    fetch::MAX_URL_BYTES,
    read::MAX_READ_LINES,
    search::{
        MAX_CONTEXT, MAX_CURSOR_BYTES, MAX_GLOB_BYTES, MAX_GLOBS, MAX_LIMIT, MAX_PER_FILE,
        MAX_QUERY_BYTES,
    },
    shell::{
        MAX_EXEC_ARG_BYTES, MAX_EXEC_ARGS, MAX_EXEC_PROGRAM_BYTES, MAX_EXEC_STDIN_BYTES,
        MAX_SHELL_TIMEOUT_SECS,
    },
    tree::{MAX_DEPTH, MAX_ENTRIES},
};
use crate::{
    catalog::{EffectClass, StaticTool, ToolHost},
    runtime::MAX_SHELL_ENV_NAMES,
};

/// The sub-agent tool. Not a [`BuiltInTool`]: it is declared only for runs
/// that may spawn (never for child sessions), and it dispatches to the
/// session layer rather than to a workspace execution.
pub(crate) const SPAWN_AGENT_TOOL: &str = "spawn_agent";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BuiltInTool {
    ReadFile,
    Tree,
    Search,
    EditFile,
    WriteFile,
    Shell,
    Exec,
    AskUser,
    Fetch,
    #[cfg(test)]
    TestDelay,
    #[cfg(test)]
    TestMutate,
    #[cfg(test)]
    TestShell,
}

impl BuiltInTool {
    const ALL: [Self; 9] = [
        Self::ReadFile,
        Self::Tree,
        Self::Search,
        Self::EditFile,
        Self::WriteFile,
        Self::Shell,
        Self::Exec,
        Self::AskUser,
        Self::Fetch,
    ];

    pub(super) fn from_name(name: &str) -> Option<Self> {
        match name {
            "read_file" => Some(Self::ReadFile),
            "tree" => Some(Self::Tree),
            "search" => Some(Self::Search),
            "edit_file" => Some(Self::EditFile),
            "write_file" => Some(Self::WriteFile),
            "shell" => Some(Self::Shell),
            "exec" => Some(Self::Exec),
            "ask_user" => Some(Self::AskUser),
            "fetch" => Some(Self::Fetch),
            #[cfg(test)]
            "__test_delay" => Some(Self::TestDelay),
            #[cfg(test)]
            "__test_mutate" => Some(Self::TestMutate),
            #[cfg(test)]
            "__test_shell" => Some(Self::TestShell),
            _ => None,
        }
    }

    fn effect(self) -> EffectClass {
        match self {
            Self::ReadFile | Self::Tree | Self::Search => EffectClass::ReadOnly,
            Self::EditFile | Self::WriteFile => EffectClass::Mutating,
            Self::Shell | Self::Exec => EffectClass::Shell,
            Self::AskUser => EffectClass::Interactive,
            Self::Fetch => EffectClass::Network,
            #[cfg(test)]
            Self::TestDelay => EffectClass::ReadOnly,
            #[cfg(test)]
            Self::TestMutate => EffectClass::Mutating,
            #[cfg(test)]
            Self::TestShell => EffectClass::Shell,
        }
    }

    fn spec(self) -> ToolSpec {
        match self {
            Self::ReadFile => ToolSpec::new(
                "read_file",
                "Read a workspace file by line range(s), or get its outline or info. The header carries the content hash; pass if_changed_since to skip unchanged content.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "ranges": { "type": "array", "maxItems": 8, "items": { "type": "string", "pattern": "^[0-9]+(-[0-9]*)?$" } },
                        "offset": { "type": "integer", "minimum": 1 },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MAX_READ_LINES, "default": 200 },
                        "mode": { "type": "string", "enum": ["lines", "outline", "info"], "default": "lines" },
                        "if_changed_since": { "type": "string", "pattern": "^h:[0-9a-f]{12}$" }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }),
            ),
            Self::Tree => ToolSpec::new(
                "tree",
                "Show a depth-bounded, ignore-aware directory tree with sizes and counts.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "default": "." },
                        "depth": { "type": "integer", "minimum": 1, "maximum": MAX_DEPTH, "default": 2 },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MAX_ENTRIES, "default": 120 },
                        "glob": { "type": "string", "maxLength": MAX_GLOB_BYTES },
                        "include_ignored": { "type": "boolean", "default": false }
                    },
                    "additionalProperties": false
                }),
            ),
            Self::Search => ToolSpec::new(
                "search",
                "Search workspace file contents or names, ignore-aware. Modes: content (default), names, definition, references. Omit cursor for the first page; pass the header's next= value to continue.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "minLength": 1, "maxLength": MAX_QUERY_BYTES },
                        "mode": { "type": "string", "enum": ["content", "names", "definition", "references"], "default": "content" },
                        "regex": { "type": "boolean", "default": false },
                        "case": { "type": "string", "enum": ["sensitive", "insensitive", "smart"], "default": "smart" },
                        "path": { "type": "string" },
                        "include": { "type": "array", "maxItems": MAX_GLOBS, "items": { "type": "string", "maxLength": MAX_GLOB_BYTES } },
                        "exclude": { "type": "array", "maxItems": MAX_GLOBS, "items": { "type": "string", "maxLength": MAX_GLOB_BYTES } },
                        "context": { "type": "integer", "minimum": 0, "maximum": MAX_CONTEXT, "default": 0 },
                        "limit": { "type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "default": 60 },
                        "max_per_file": { "type": "integer", "minimum": 1, "maximum": MAX_PER_FILE, "default": 10 },
                        "include_ignored": { "type": "boolean", "default": false },
                        "cursor": { "type": "string", "maxLength": MAX_CURSOR_BYTES, "description": "Omit for the first page. Only the exact next= value from a previous search result continues it." }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
            ),
            Self::EditFile => ToolSpec::new(
                "edit_file",
                "Apply one or more edits to files read earlier in this session, atomically. Each edit replaces old with new, or inserts new before/after an anchor. Fuzzy whitespace/indent matching is on unless fuzzy=false; the result names via=<strategy> when it was used. Set dry_run to preview.",
                json!({
                    "type": "object",
                    "properties": {
                        "edits": {
                            "type": "array", "minItems": 1, "maxItems": MAX_EDITS,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "path": { "type": "string" },
                                    "old": { "type": "string" },
                                    "new": { "type": "string" },
                                    "insert_before": { "type": "string" },
                                    "insert_after": { "type": "string" },
                                    "replace_all": { "type": "boolean", "default": false },
                                    "if_hash": { "type": "string", "pattern": "^h:[0-9a-f]{12}$" }
                                },
                                "required": ["path"],
                                "additionalProperties": false
                            }
                        },
                        "fuzzy": { "type": "boolean", "default": true },
                        "dry_run": { "type": "boolean", "default": false }
                    },
                    "required": ["edits"],
                    "additionalProperties": false
                }),
            ),
            Self::WriteFile => ToolSpec::new(
                "write_file",
                "Create a workspace file (parents are created), or fully overwrite one read earlier in this session. create_only fails if it exists; if_hash proves currency without a prior read.",
                json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                        "create_only": { "type": "boolean", "default": false },
                        "if_hash": { "type": "string", "pattern": "^h:[0-9a-f]{12}$" }
                    },
                    "required": ["path", "content"],
                    "additionalProperties": false
                }),
            ),
            Self::Shell => ToolSpec::new(
                "shell",
                "Run one shell command in the workspace via `sh -c`, capturing combined stdout and stderr (head+tail; the full output is stored). The child starts from a cleared environment plus PATH HOME LANG TERM TMPDIR; name allowlisted variables in env. Timeout 120 s by default; the process group is killed on timeout or cancel.",
                json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string", "minLength": 1 },
                        "cwd": {
                            "type": "string",
                            "description": "Working directory relative to the workspace root; defaults to the root."
                        },
                        "timeout_seconds": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_SHELL_TIMEOUT_SECS
                        },
                        "env": {
                            "type": "array",
                            "maxItems": MAX_SHELL_ENV_NAMES,
                            "items": { "type": "string", "pattern": "^[A-Za-z_][A-Za-z0-9_]*$" }
                        }
                    },
                    "required": ["command"],
                    "additionalProperties": false
                }),
            ),
            Self::Exec => ToolSpec::new(
                "exec",
                "Run one program with an argument list; no shell interpretation (no quoting, globbing, $, or pipes). Prefer it over shell for a single program such as `cargo test -p x` or `python -m pytest tests/x.py`; reserve shell for pipelines. Same environment, timeout, and output rules as shell.",
                json!({
                    "type": "object",
                    "properties": {
                        "program": { "type": "string", "minLength": 1, "maxLength": MAX_EXEC_PROGRAM_BYTES },
                        "args": {
                            "type": "array",
                            "maxItems": MAX_EXEC_ARGS,
                            "items": { "type": "string", "maxLength": MAX_EXEC_ARG_BYTES }
                        },
                        "cwd": { "type": "string" },
                        "stdin": { "type": "string", "maxLength": MAX_EXEC_STDIN_BYTES },
                        "timeout_seconds": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": MAX_SHELL_TIMEOUT_SECS
                        },
                        "env": {
                            "type": "array",
                            "maxItems": MAX_SHELL_ENV_NAMES,
                            "items": { "type": "string", "pattern": "^[A-Za-z_][A-Za-z0-9_]*$" }
                        }
                    },
                    "required": ["program"],
                    "additionalProperties": false
                }),
            ),
            Self::AskUser => ToolSpec::new(
                "ask_user",
                "Ask the user 1-4 questions and wait for their answers. Use only when the task is genuinely ambiguous and a wrong guess would be costly; offer 2-6 concrete options per question, or free_text for open answers.",
                json!({
                    "type": "object",
                    "properties": {
                        "questions": {
                            "type": "array",
                            "minItems": 1,
                            "maxItems": MAX_QUESTIONS,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "prompt": { "type": "string", "minLength": 1, "maxLength": MAX_QUESTION_BYTES },
                                    "options": {
                                        "type": "array",
                                        "minItems": MIN_OPTIONS,
                                        "maxItems": MAX_OPTIONS,
                                        "items": { "type": "string", "minLength": 1, "maxLength": MAX_OPTION_BYTES }
                                    },
                                    "free_text": { "type": "boolean", "default": false }
                                },
                                "required": ["prompt"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["questions"],
                    "additionalProperties": false
                }),
            ),
            Self::Fetch => ToolSpec::new(
                "fetch",
                "Fetch a public http(s) URL (GET, or method=HEAD for headers only). HTML is converted to markdown, JSON is formatted; the body is bounded and spills when long. Private, link-local, and managed-denied hosts are refused.",
                json!({
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "minLength": 1, "maxLength": MAX_URL_BYTES },
                        "method": { "type": "string", "enum": ["GET", "HEAD"], "default": "GET" }
                    },
                    "required": ["url"],
                    "additionalProperties": false
                }),
            ),
            #[cfg(test)]
            Self::TestDelay | Self::TestMutate | Self::TestShell => {
                unreachable!("test tools are not advertised")
            }
        }
    }
}

/// Byte bound on the `spawn_agent` declaration (name, description, schema).
/// The roster is bounded upstream; this catches a description that grows
/// past what every turn should pay for.
pub(crate) const MAX_SPAWN_AGENT_SCHEMA_BYTES: usize = 2 * 1024;

/// The declaration for [`SPAWN_AGENT_TOOL`]. Kept out of [`specs`] because it
/// joins the tool list only when the run may spawn: child sessions and
/// session-less runs never see it. With a roster, `role` is the model's
/// primary selector and the exact `model` override is limited to roster
/// routes; without one, `model` spans every authenticated route as before.
pub(crate) fn spawn_agent_spec(model_routes: &[String], delegation: &DelegationRoster) -> ToolSpec {
    let mut properties = serde_json::Map::from_iter([(
        "task".to_owned(),
        json!({
            "type": "string",
            "minLength": 1,
            "description": "A complete, self-contained brief for the sub-agent."
        }),
    )]);
    let has_roster = !delegation.roster.is_empty();
    if has_roster {
        let roles: Vec<&str> = {
            let mut roles: Vec<DelegationRole> =
                delegation.roster.iter().map(|entry| entry.role).collect();
            roles.sort();
            roles.dedup();
            roles.into_iter().map(DelegationRole::as_str).collect()
        };
        properties.insert(
            "role".to_owned(),
            json!({
                "type": "string",
                "enum": roles,
                "description": format!(
                    "Which roster role should run this task. Omit to use the default ({}). \
                     Pick fast for lookups and breadth, balanced for ordinary work, strong for \
                     hard reasoning; the system prompt lists each role's route and relative cost.",
                    delegation.default_role.as_str()
                )
            }),
        );
    }
    // Write authority is advertised only when the roster permits it; a model
    // never sees an option the spawner would refuse.
    if delegation.write_children {
        properties.insert(
            "authority".to_owned(),
            json!({
                "type": "string",
                "enum": ["read", "write"],
                "description": "read (default): the sub-agent may only read the workspace. write: it may edit files and run commands, but every such action is held and adjudicated by the reviewer model before it runs, and only one write sub-agent runs at a time. Request write only when the task itself requires changing the workspace."
            }),
        );
    }
    let override_routes: Vec<&str> = if has_roster {
        delegation
            .roster
            .iter()
            .map(|entry| entry.route.as_str())
            .collect()
    } else {
        model_routes.iter().map(String::as_str).collect()
    };
    if !override_routes.is_empty() {
        properties.insert(
            "model".to_owned(),
            json!({
                "type": "string",
                "enum": override_routes,
                "description": if has_roster {
                    "Exact roster route override. Omit by default and choose by role instead. Set only when the user explicitly requests one of these exact routes; never guess or translate providers."
                } else {
                    "Exact authenticated provider/model override. Omit by default to use QQ's configured worker model or this session's selected model. Set only when the user explicitly requests one of these exact routes; never guess or translate providers."
                }
            }),
        );
    }
    let description = if has_roster {
        "Delegate one self-contained task to a read-only sub-agent in this workspace and receive \
         only its final answer. Worth it when the raw evidence would dwarf the distilled answer \
         and you will not need that evidence verbatim later; several independent questions can be \
         delegated in parallel. Single reads, searches, and quick lookups are cheaper inline. The \
         task brief must carry everything the sub-agent needs: it starts with no other context. \
         Choose the sub-agent by role (see Delegation in the system prompt for each role's route \
         and relative cost); omit role for the default. Set model only when the user explicitly \
         requests an exact roster route; never guess, translate, or invent a route."
    } else {
        "Delegate one self-contained task to a read-only sub-agent in this workspace and receive \
         only its final answer. Worth it when the raw evidence would dwarf the distilled answer \
         and you will not need that evidence verbatim later; several independent questions can be \
         delegated in parallel. Single reads, searches, and quick lookups are cheaper inline. The \
         task brief must carry everything the sub-agent needs: it starts with no other context. \
         Omit model by default so QQ uses its configured worker model or the current session's \
         selected model. Set model only when the user explicitly requests an exact provider/model \
         route listed by this tool; never guess, translate, or invent a route."
    };
    ToolSpec::new(
        SPAWN_AGENT_TOOL,
        description,
        serde_json::Value::Object(serde_json::Map::from_iter([
            ("type".to_owned(), json!("object")),
            (
                "properties".to_owned(),
                serde_json::Value::Object(properties),
            ),
            ("required".to_owned(), json!(["task"])),
            ("additionalProperties".to_owned(), json!(false)),
        ])),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SpawnAgentArgs {
    pub(crate) task: String,
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) role: Option<DelegationRole>,
    #[serde(default)]
    pub(crate) authority: ChildAuthority,
}

pub(crate) fn specs() -> Vec<ToolSpec> {
    static SPECS: OnceLock<Vec<ToolSpec>> = OnceLock::new();
    SPECS
        .get_or_init(|| {
            BuiltInTool::ALL
                .into_iter()
                .map(BuiltInTool::spec)
                .collect()
        })
        .clone()
}

/// The built-in tools as the catalog compiler receives them, each carrying
/// the effect policy will classify it by.
pub(crate) fn static_tools() -> Vec<StaticTool> {
    static TOOLS: OnceLock<Vec<StaticTool>> = OnceLock::new();
    TOOLS
        .get_or_init(|| {
            specs()
                .into_iter()
                .zip(BuiltInTool::ALL)
                .map(|(spec, tool)| StaticTool::new(spec, ToolHost::BuiltIn, tool.effect()))
                .collect()
        })
        .clone()
}

/// The effect of a test-only tool, which dispatch executes but the catalog
/// never advertises.
#[cfg(test)]
pub(crate) fn test_tool_effect(name: &str) -> Option<EffectClass> {
    match BuiltInTool::from_name(name)? {
        tool @ (BuiltInTool::TestDelay | BuiltInTool::TestMutate | BuiltInTool::TestShell) => {
            Some(tool.effect())
        }
        _ => None,
    }
}
