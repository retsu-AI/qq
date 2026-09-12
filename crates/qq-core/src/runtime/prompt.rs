use std::path::Path;

use qq_protocol::{ContentHash, DelegationRoster, PromptVersion};
use qq_provider::ToolSpec;

use crate::{
    hosts::{EMBEDDED_TOOL_PREFIX, MCP_TOOL_PREFIX},
    tools::SPAWN_AGENT_TOOL,
    workspace::{SelectedGuidance, WorkspaceInstructions},
};

pub(crate) const AGENT_PROMPT_VERSION: PromptVersion = match PromptVersion::new(10) {
    Some(version) => version,
    None => panic!("agent prompt version must be nonzero"),
};

/// Version 10 of the base agent prompt. The text is versioned in code, not
/// configuration: bump this note and review the diff whenever it changes.
///
/// `tool_index` is the progressive-exposure index of external tools not yet
/// callable; `roster_text` is the compiled delegation roster block (routes,
/// roles, relative cost) when one is configured; `skill_index` lists
/// disclosed skills the model may load.
/// Compiled prompt sections a run appends to the base prompt: none of them
/// is authored per turn.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct PromptSections<'a> {
    /// Progressive-exposure index of external tools not yet callable.
    pub(crate) tool_index: Option<&'a str>,
    /// The rendered delegation roster (see `delegation_roster_text`).
    pub(crate) roster: Option<&'a str>,
    /// Disclosed skills the model may load.
    pub(crate) skill_index: Option<&'a str>,
}

pub(crate) fn agent_system_prompt(
    workspace: &Path,
    specs: &[ToolSpec],
    sections: PromptSections<'_>,
    workspace_instructions: &WorkspaceInstructions,
    persona: Option<&crate::plan::Persona>,
    selected_guidance: Option<&SelectedGuidance>,
) -> String {
    let PromptSections {
        tool_index,
        roster: roster_text,
        skill_index,
    } = sections;
    let mut tool_names = String::new();
    let mut has_external = tool_index.is_some();
    let mut has_spawn = false;
    for spec in specs {
        if !tool_names.is_empty() {
            tool_names.push_str(", ");
        }
        tool_names.push_str(spec.name());
        has_external |= spec.name().starts_with(MCP_TOOL_PREFIX)
            || spec.name().starts_with(EMBEDDED_TOOL_PREFIX);
        has_spawn |= spec.name() == SPAWN_AGENT_TOOL;
    }
    let mcp_note = if has_external {
        " Tools named mcp__<server>__<tool> or ext__<host>__<tool> call external tool hosts, \
         execute outside the workspace, and may require user approval."
    } else {
        ""
    };
    // With a roster the model chooses by role and the roster line (built once
    // at plan compile) names each role's route and relative cost; without one
    // the legacy worker/parent fallback text applies.
    let model_guidance = match (has_spawn, roster_text) {
        (true, Some(roster)) => format!(
            "- Choose the sub-agent by spawn_agent's role argument; omit it for the default role. \
             Set model only when the user explicitly requests an exact roster route; never \
             guess, translate, or invent one.\n{roster}\n"
        ),
        (true, None) => {
            "- Omit spawn_agent's model argument by default. QQ then uses the configured worker \
         model or this session's persisted selected model, including its authenticated provider. \
         Set model only when the user explicitly requests an exact provider/model route; never \
         guess, translate, or invent one.\n"
                .to_owned()
        }
        (false, _) => String::new(),
    };
    let spawn_section = if has_spawn {
        format!(
            "\n\nDelegation:\n\
         - spawn_agent runs a one-shot read-only sub-agent in this workspace from a \
         self-contained task brief and returns only its final answer.\n\
{model_guidance}\
         - Delegate when all three hold: the raw evidence would dwarf the distilled answer, \
         you will not need that evidence verbatim later, and the task needs no mid-flight \
         steering.\n\
         - Default to working inline: single reads, searches, and quick lookups are never \
         worth a sub-agent.\n\
         - Exception: several independent questions are worth delegating even when each is \
         small, because sub-agents run concurrently."
        )
    } else {
        String::new()
    };
    let mut prompt = format!(
        "You are QQ, a coding agent operating in the workspace rooted at {root}.\n\
         \n\
         Available tools: {tool_names}. read_file, tree, search, and search_history are read-only; \
         edit_file and write_file modify workspace files and may require user approval; \
         shell runs one command in the workspace with a bounded timeout and may require user approval.{mcp_note}\n\
         \n\
         Working conventions:\n\
         - Determine observable completion criteria from the user's request before acting.\n\
         - Read a file with read_file before editing or overwriting it; edits without a prior read in this session are rejected.\n\
         - Inspect existing state before changing it and preserve unrelated work.\n\
         - Prefer search over guessing file paths, and search/tree over shell grep, rg, find, and ls; search groups matches by file as L<n>: text and its header carries next=<cursor> when more exist.\n\
         - Give every tool path relative to the workspace root; absolute paths are rejected.\n\
         - Before changing files below a subdirectory, inspect each directory from the workspace root to the target for AGENTS.md; when AGENTS.md is absent at one scope, check CLAUDE.md. Apply selected instructions root-to-leaf, with more-specific instructions taking precedence.\n\
         - Implement requested changes rather than stopping at analysis unless the user requested analysis-only work.\n\
         - Treat failed tools and tests as evidence: diagnose them and continue when a safe path remains.\n\
         - Run the narrowest relevant verification before broader checks.\n\
         - Do not claim success without evidence from the resulting state.\n\
         - Report remaining failures and uncertainty honestly.\n\
         - Respect explicit time, token, cost, and safety budgets.\n\
         - Prefer edit_file and write_file over shell for changing files.{spawn_section}",
        root = workspace.display(),
    );
    if let Some(index) = tool_index {
        prompt.push_str("\n\n");
        prompt.push_str(index.trim_end());
    }
    if let Some(index) = skill_index {
        prompt.push_str("\n\n");
        prompt.push_str(index.trim_end());
    }
    workspace_instructions.append_to_prompt(&mut prompt);
    if let Some(persona) = persona {
        persona.append_to_prompt(&mut prompt);
    }
    if let Some(guidance) = selected_guidance {
        guidance.append_to_prompt(&mut prompt);
    }
    prompt
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolSchemaMeasurement {
    pub(crate) hash: ContentHash,
    pub(crate) bytes: u64,
}

pub(crate) fn tool_schema_measurement(specs: &[ToolSpec]) -> ToolSchemaMeasurement {
    let schemas: Vec<String> = specs
        .iter()
        .map(|spec| spec.input_schema().to_string())
        .collect();
    measure_tool_schemas(specs.iter().zip(schemas.iter().map(String::as_str)))
}

/// Measures declarations whose schemas are already serialized, so callers
/// that serialized them once (the catalog compiler) need not do it again.
pub(crate) fn measure_tool_schemas<'a>(
    specs: impl Iterator<Item = (&'a ToolSpec, &'a str)>,
) -> ToolSchemaMeasurement {
    use sha2::{Digest, Sha256};

    let mut digest = Sha256::new();
    let mut measured_bytes = 0_u64;
    for (spec, schema) in specs {
        for bytes in [spec.name().as_bytes(), spec.description().as_bytes()] {
            digest.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
            digest.update(bytes);
            measured_bytes =
                measured_bytes.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        }
        digest.update(
            u64::try_from(schema.len())
                .unwrap_or(u64::MAX)
                .to_be_bytes(),
        );
        digest.update(schema.as_bytes());
        measured_bytes = measured_bytes
            .saturating_add(u64::try_from(schema.len()).unwrap_or(u64::MAX))
            .saturating_add(32);
    }
    ToolSchemaMeasurement {
        hash: ContentHash::from_bytes(digest.finalize().into()),
        bytes: measured_bytes,
    }
}

/// Renders the delegation roster for the system prompt: the spawning model's
/// own identity first (so it can judge relative cost), then every roster
/// entry with its role, route, context window, relative cost, and note.
/// Built once at plan compile; the run loop only concatenates it.
pub(crate) fn delegation_roster_text(
    current_route: &str,
    current_context_window: Option<u32>,
    roster: &DelegationRoster,
) -> Option<String> {
    if roster.roster.is_empty() {
        return None;
    }
    let mut text = String::with_capacity(256);
    text.push_str("         - You are running as ");
    text.push_str(current_route);
    if let Some(window) = current_context_window {
        text.push_str(" (");
        push_tokens(&mut text, window);
        text.push_str(" context)");
    }
    text.push_str(". Roster (default role: ");
    text.push_str(roster.default_role.as_str());
    text.push_str("):\n");
    for entry in &roster.roster {
        text.push_str("           - ");
        text.push_str(entry.role.as_str());
        text.push_str(": ");
        text.push_str(&entry.route);
        let mut details: Vec<String> = Vec::with_capacity(3);
        if let Some(window) = entry.context_window {
            let mut rendered = String::new();
            push_tokens(&mut rendered, window);
            rendered.push_str(" context");
            details.push(rendered);
        }
        match entry.relative_cost_permille {
            Some(1000) => details.push("same cost as you".to_owned()),
            Some(permille) if permille < 1000 => {
                details.push(format!("~{}% of your cost", permille.div_ceil(10)));
            }
            Some(permille) => {
                details.push(format!("~{:.1}x your cost", f64::from(permille) / 1000.0));
            }
            None => {}
        }
        if let Some(note) = &entry.note {
            details.push(note.clone());
        }
        if !details.is_empty() {
            text.push_str(" — ");
            text.push_str(&details.join("; "));
        }
        text.push('\n');
    }
    text.truncate(text.trim_end().len());
    Some(text)
}

fn push_tokens(text: &mut String, tokens: u32) {
    use std::fmt::Write as _;
    if tokens >= 1_000_000 && tokens.is_multiple_of(100_000) {
        let _ = write!(text, "{}M", f64::from(tokens) / 1_000_000.0);
    } else if tokens >= 1_000 && tokens.is_multiple_of(1_000) {
        let _ = write!(text, "{}k", tokens / 1_000);
    } else {
        let _ = write!(text, "{tokens}");
    }
}
