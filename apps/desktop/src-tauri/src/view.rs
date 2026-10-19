//! Serializable view models the UI renders.
//!
//! The reducer (`qq_client::state`) owns session state; this module only
//! projects it. Field names stay `snake_case` so DTOs and the protocol types
//! they embed read the same on the TypeScript side (`src/lib/types.ts`).

use std::{collections::HashMap, path::Path};

use qq_client::state::{Group, ModelOption, SessionStore, SessionView};
use qq_protocol::{
    ApprovalMode, GuidanceKind, MessageRole, MessageSnapshot, MessageState, RunActivity, RunId,
    RunOutcome, SessionId, SessionStatus, SideQuestionState, SkillSummary, TokenUsage,
    ToolCallSnapshot, ToolCallState,
};
use serde::Serialize;
#[cfg(test)]
use ts_rs::TS;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(TS))]
#[serde(rename_all = "snake_case")]
pub enum ConnectionDto {
    Starting,
    Connecting,
    Replaying,
    Live,
    Offline,
    Failed,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(TS))]
pub struct NoticeDto {
    #[cfg_attr(test, ts(type = "\"info\" | \"warning\" | \"error\""))]
    pub level: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(TS))]
pub struct ModelDto {
    pub id: String,
    pub provider: String,
    pub model: String,
    pub name: Option<String>,
    pub context_window: Option<u32>,
    #[cfg_attr(test, ts(type = "string[]"))]
    pub reasoning_efforts: Vec<qq_protocol::ReasoningEffort>,
}

impl ModelDto {
    pub fn from_option(option: &ModelOption) -> Self {
        Self {
            id: model_id(option),
            provider: option.provider.clone(),
            model: option.model.clone(),
            name: option.name.clone(),
            context_window: option.context_window,
            reasoning_efforts: option.reasoning_efforts.clone(),
        }
    }
}

/// Stable key for a catalog entry: the selection's route when it has one.
pub fn model_id(option: &ModelOption) -> String {
    option
        .selection
        .model
        .clone()
        .unwrap_or_else(|| format!("{}/{}", option.provider, option.model))
}

/// One row of the session list.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(TS))]
pub struct SessionRowDto {
    #[cfg_attr(test, ts(type = "string"))]
    pub id: SessionId,
    #[cfg_attr(test, ts(type = "string | null"))]
    pub parent_id: Option<SessionId>,
    pub depth: usize,
    pub title: String,
    #[cfg_attr(test, ts(type = "Group"))]
    pub group: &'static str,
    #[cfg_attr(test, ts(type = "SessionStatus"))]
    pub status: SessionStatus,
    #[cfg_attr(test, ts(type = "RunActivity | null"))]
    pub activity: Option<RunActivity>,
    pub active_tool: Option<String>,
    pub tail: String,
    pub unread: u32,
    pub awaiting_approval: usize,
    pub queued_prompts: u16,
    pub model: Option<String>,
    #[cfg_attr(test, ts(type = "number"))]
    pub updated_at_ms: u64,
    pub failed: bool,
}

/// Everything the window shows for one open project.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[cfg_attr(test, derive(TS))]
pub struct ProjectStateDto {
    pub project_id: String,
    pub path: String,
    pub name: String,
    pub connection: ConnectionDto,
    pub error: Option<String>,
    pub owns_server: bool,
    pub sessions: Vec<SessionRowDto>,
    #[cfg_attr(test, ts(type = "string | null"))]
    pub focused: Option<SessionId>,
    pub transcript: Option<TranscriptDto>,
    pub models: Vec<ModelDto>,
    pub selected_model: Option<String>,
    pub can_steer: bool,
    pub can_interrupt: bool,
    pub profiles: Vec<String>,
    #[cfg_attr(
        test,
        ts(
            type = "Array<{ name: string; generation: number; tool_count: number; ready: boolean; message?: string }>"
        )
    )]
    pub tool_hosts: Vec<qq_protocol::ToolHostSummary>,
    /// The workspace's commands and skills, invoked as `/name`.
    pub skills: Vec<SkillDto>,
    /// The server answers side questions during a run.
    pub can_side_question: bool,
    pub notice: Option<NoticeDto>,
}

/// One indexed command or skill document.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(TS))]
pub struct SkillDto {
    pub name: String,
    #[cfg_attr(test, ts(type = "\"command\" | \"skill\""))]
    pub kind: &'static str,
    pub description: String,
}

impl SkillDto {
    pub fn from_summary(summary: &SkillSummary) -> Self {
        Self {
            name: summary.name.clone(),
            kind: match summary.kind {
                GuidanceKind::Command => "command",
                GuidanceKind::Skill => "skill",
            },
            description: summary.description.clone(),
        }
    }
}

/// A question answered beside the run, never part of its context.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(TS))]
pub struct SideQuestionDto {
    #[cfg_attr(test, ts(type = "string"))]
    pub id: RunId,
    #[cfg_attr(test, ts(type = "string"))]
    pub thread_id: RunId,
    pub question: String,
    pub answer: String,
    #[cfg_attr(
        test,
        ts(
            type = "\"running\" | \"completed\" | \"failed\" | \"cancelled\" | \"timed_out\" | \"interrupted\""
        )
    )]
    pub state: SideQuestionState,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[cfg_attr(test, derive(TS))]
pub struct TranscriptDto {
    #[cfg_attr(test, ts(type = "string"))]
    pub session_id: SessionId,
    pub title: String,
    pub loading: bool,
    #[cfg_attr(test, ts(type = "SessionStatus"))]
    pub status: SessionStatus,
    #[cfg_attr(test, ts(type = "RunActivity | null"))]
    pub activity: Option<RunActivity>,
    #[cfg_attr(test, ts(type = "string | null"))]
    pub active_run_id: Option<RunId>,
    pub model: Option<String>,
    #[cfg_attr(test, ts(type = "ApprovalMode"))]
    pub approval_mode: ApprovalMode,
    #[cfg_attr(test, ts(type = "string"))]
    pub profile: qq_protocol::AgentProfileId,
    #[cfg_attr(test, ts(type = "string | null"))]
    pub reasoning_effort: Option<qq_protocol::ReasoningEffort>,
    #[cfg_attr(test, ts(type = "string | null"))]
    pub approval_delegate: Option<qq_protocol::ApprovalDelegate>,
    #[cfg_attr(test, ts(type = "number | null"))]
    pub context_tokens: Option<u64>,
    pub context_window: Option<u32>,
    #[cfg_attr(test, ts(type = "number | null"))]
    pub cost_usd_nanos: Option<u64>,
    pub queued_prompts: u16,
    pub items: Vec<ItemDto>,
    /// Oldest first; the server keeps a bounded recent window.
    pub side_questions: Vec<SideQuestionDto>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[cfg_attr(test, derive(TS))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ItemDto {
    User {
        id: String,
        text: String,
        #[cfg_attr(test, ts(type = "MessageState"))]
        state: MessageState,
        steering: bool,
    },
    Assistant {
        id: String,
        text: String,
        refusal: String,
        #[cfg_attr(test, ts(type = "MessageState"))]
        state: MessageState,
        truncated: bool,
    },
    Reasoning {
        id: String,
        text: String,
        streaming: bool,
    },
    Tool(Box<ToolDto>),
    RunEnd {
        id: String,
        #[cfg_attr(test, ts(type = "RunOutcome | null"))]
        outcome: Option<RunOutcome>,
        #[cfg_attr(test, ts(type = "number | null"))]
        duration_ms: Option<u64>,
        #[cfg_attr(test, ts(type = "TokenUsage | null"))]
        usage: Option<TokenUsage>,
        #[cfg_attr(test, ts(type = "number | null"))]
        cost_usd_nanos: Option<u64>,
        tool_calls: u32,
    },
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[cfg_attr(test, derive(TS))]
pub struct ToolDto {
    pub id: String,
    #[cfg_attr(test, ts(type = "string"))]
    pub run_id: RunId,
    pub name: String,
    pub arguments: String,
    #[cfg_attr(test, ts(type = "ToolCallState"))]
    pub state: ToolCallState,
    pub result: Option<String>,
    pub is_error: bool,
    pub diff: Option<DiffDto>,
    pub live_output: Option<String>,
    #[cfg_attr(test, ts(type = "number | null"))]
    pub duration_ms: Option<u64>,
    pub settled: Option<String>,
    pub approval: Option<ApprovalDto>,
    /// The child session a `spawn_agent` call created, for linking.
    #[cfg_attr(test, ts(type = "string | null"))]
    pub child_session: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[cfg_attr(test, derive(TS))]
pub struct DiffDto {
    pub path: String,
    pub diff: String,
}

/// What a held call would do, as the server described it.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[cfg_attr(test, derive(TS))]
pub struct ApprovalDto {
    #[cfg_attr(test, ts(type = "ShellPreview | null"))]
    pub shell: Option<serde_json::Value>,
    #[cfg_attr(test, ts(type = "{ path: string; diff: string } | null"))]
    pub edit: Option<serde_json::Value>,
    #[cfg_attr(test, ts(type = "{ questions: Question[] } | null"))]
    pub question: Option<serde_json::Value>,
    #[cfg_attr(
        test,
        ts(type = "{ host: string; url: string; method?: string } | null")
    )]
    pub fetch: Option<serde_json::Value>,
    pub escalation: Option<String>,
    /// Whether "for session"/"for workspace" grants can be offered.
    pub grantable: bool,
    pub grant_label: String,
}

/// A path as people read it, without Windows' verbatim `\\?\` prefix. Only
/// for display: the server is always sent the canonical form.
pub fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
}

pub fn display_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || display_path(path),
        |name| name.to_string_lossy().into_owned(),
    )
}

pub fn group_label(group: Group) -> &'static str {
    match group {
        Group::NeedsYou => "needs_you",
        Group::Working => "working",
        Group::Idle => "idle",
        Group::Done => "done",
    }
}

pub fn session_rows(store: &SessionStore) -> Vec<SessionRowDto> {
    let mut rows = Vec::with_capacity(store.len());
    for id in store.thread_order() {
        let Some(view) = store.get(id) else { continue };
        rows.push(SessionRowDto {
            id: *id,
            parent_id: view.summary.parent_id,
            depth: store.depth(*id),
            title: display_title(view),
            group: group_label(view.group()),
            status: view.summary.status,
            activity: view.activity.map(|(_, activity)| activity),
            active_tool: view.live.active_tool.clone(),
            tail: view.live.tail.clone(),
            unread: view.unread,
            awaiting_approval: view.live.awaiting_approval.len(),
            queued_prompts: view.summary.queued_prompts,
            model: view.summary.model.clone(),
            updated_at_ms: view.summary.updated_at_ms,
            failed: matches!(view.summary.last_outcome, Some(RunOutcome::Failed { .. })),
        });
    }
    rows
}

pub fn display_title(view: &SessionView) -> String {
    let title = view.summary.title.trim();
    if title.is_empty() {
        "New session".to_owned()
    } else {
        title.to_owned()
    }
}

pub fn transcript(store: &SessionStore, id: SessionId) -> Option<TranscriptDto> {
    let view = store.get(&id)?;
    let summary = &view.summary;
    let items = match (&view.messages, &view.tool_calls) {
        (Some(messages), Some(tool_calls)) => transcript_items(store, view, messages, tool_calls),
        _ => Vec::new(),
    };
    Some(TranscriptDto {
        session_id: id,
        title: display_title(view),
        loading: !view.is_warm(),
        status: summary.status,
        activity: view.activity.map(|(_, activity)| activity),
        active_run_id: summary.active_run_id,
        model: summary.model.clone(),
        approval_mode: summary.approval_mode,
        profile: summary.profile.clone(),
        reasoning_effort: summary.reasoning_effort,
        approval_delegate: summary.approval_delegate,
        context_tokens: summary.context_tokens,
        context_window: view.context_window,
        cost_usd_nanos: summary.estimated_cost_usd_nanos,
        queued_prompts: summary.queued_prompts,
        items,
        side_questions: view
            .side_questions
            .iter()
            .map(|side| SideQuestionDto {
                id: side.id,
                thread_id: side.thread_id,
                question: side.question.clone(),
                answer: side.answer.clone(),
                state: side.state,
            })
            .collect(),
    })
}

/// Interleave a session's messages and tool calls in reading order: each
/// run's prompt, its reasoning, then each model turn's text followed by the
/// calls that turn made, and a completion line once the run settles.
fn transcript_items(
    store: &SessionStore,
    view: &SessionView,
    messages: &[MessageSnapshot],
    tool_calls: &[ToolCallSnapshot],
) -> Vec<ItemDto> {
    // Runs in the order they first appear; tool calls of a run the message
    // window no longer holds are ordered after it rather than dropped.
    let mut run_order: HashMap<RunId, usize> = HashMap::new();
    for message in messages {
        let next = run_order.len();
        run_order.entry(message.run_id).or_insert(next);
    }
    for call in tool_calls {
        let next = run_order.len();
        run_order.entry(call.run_id).or_insert(next);
    }

    // (run, turn, rank, position) sorts each run as prompt, reasoning, then
    // per turn its text and calls; the run's completion line sorts last.
    let mut keyed: Vec<((usize, u32, u8, usize), ItemDto)> = Vec::new();
    for (position, message) in messages.iter().enumerate() {
        let run = run_order[&message.run_id];
        match message.role {
            MessageRole::User => keyed.push((
                (run, message.turn_ordinal, 0, position),
                ItemDto::User {
                    id: message.id.to_string(),
                    text: message.output.clone(),
                    state: message.state,
                    steering: message.steering,
                },
            )),
            MessageRole::Assistant => {
                if message.output.is_empty()
                    && message.refusal.is_empty()
                    && message.state != MessageState::Streaming
                {
                    continue;
                }
                keyed.push((
                    (run, message.turn_ordinal.max(1), 2, position),
                    ItemDto::Assistant {
                        id: message.id.to_string(),
                        text: message.output.clone(),
                        refusal: message.refusal.clone(),
                        state: message.state,
                        truncated: message.truncated,
                    },
                ));
            }
        }
    }
    for (run_id, &run) in &run_order {
        if let Some(reasoning) = view.reasoning.get(run_id)
            && !reasoning.text.trim().is_empty()
        {
            keyed.push((
                (run, 0, 1, 0),
                ItemDto::Reasoning {
                    id: format!("reasoning-{run_id}"),
                    text: reasoning.text.clone(),
                    streaming: reasoning.streaming,
                },
            ));
        }
        if let Some(stats) = view.runs.get(run_id)
            && stats.outcome.is_some()
        {
            keyed.push((
                (run, u32::MAX, u8::MAX, 0),
                ItemDto::RunEnd {
                    id: format!("end-{run_id}"),
                    outcome: stats.outcome.clone(),
                    duration_ms: stats
                        .started_at_ms
                        .zip(stats.finished_at_ms)
                        .map(|(start, end)| end.saturating_sub(start)),
                    usage: stats.usage,
                    cost_usd_nanos: stats.cost_usd_nanos.or(stats.live_cost_usd_nanos),
                    tool_calls: stats.tool_calls,
                },
            ));
        }
    }
    for call in tool_calls {
        let run = run_order[&call.run_id];
        keyed.push((
            (
                run,
                call.turn_ordinal.max(1),
                3,
                usize::from(call.call_ordinal),
            ),
            ItemDto::Tool(Box::new(tool_item(store, view, call))),
        ));
    }
    keyed.sort_by_key(|(key, _)| *key);
    keyed.into_iter().map(|(_, item)| item).collect()
}

fn tool_item(store: &SessionStore, view: &SessionView, call: &ToolCallSnapshot) -> ToolDto {
    let timing = view.tool_timing.get(&call.id);
    let approval = (call.state == ToolCallState::AwaitingApproval).then(|| {
        let preview = view.approval_previews.get(&call.id);
        let grant = crate::project::approval_grant(call, preview);
        ApprovalDto {
            shell: preview
                .and_then(|preview| preview.shell.as_ref())
                .and_then(|shell| serde_json::to_value(shell).ok()),
            edit: preview
                .and_then(|preview| preview.edit.as_ref())
                .and_then(|edit| serde_json::to_value(edit).ok()),
            question: preview
                .and_then(|preview| preview.question.as_ref())
                .and_then(|question| serde_json::to_value(question).ok()),
            fetch: preview
                .and_then(|preview| preview.fetch.as_ref())
                .and_then(|fetch| serde_json::to_value(fetch).ok()),
            escalation: preview
                .and_then(|preview| preview.escalated.as_ref())
                .map(|escalated| escalated.reason.clone()),
            grantable: crate::project::grant_recordable(&grant),
            grant_label: crate::project::grant_label(&grant),
        }
    });
    let diff = call.display.as_ref().map(|display| match display {
        qq_protocol::ToolCallDisplay::Diff { path, diff } => DiffDto {
            path: path.clone(),
            diff: diff.clone(),
        },
    });
    ToolDto {
        id: call.id.to_string(),
        run_id: call.run_id,
        name: call.name.clone(),
        arguments: call.arguments.clone(),
        state: call.state,
        result: call.result.clone(),
        is_error: call.is_error,
        diff,
        live_output: view.live_tool_output.get(&call.id).cloned(),
        duration_ms: timing.and_then(|timing| {
            timing
                .started_at_ms
                .zip(timing.finished_at_ms)
                .map(|(start, end)| end.saturating_sub(start))
        }),
        settled: timing
            .and_then(|timing| timing.settled)
            .and_then(|settled| settled.label()),
        approval,
        child_session: store.child_spawned_by(call.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typescript_view_contract_is_current() {
        let mut generated = String::from(
            "// Generated from src-tauri/src/view.rs. Run QQ_EXPORT_TYPES=1 cargo test typescript_view_contract_is_current.\nimport type { SessionStatus, RunActivity, MessageState, ToolCallState, ApprovalMode, Group, RunOutcome, TokenUsage, ShellPreview, Question } from './types';\n\n",
        );
        for declaration in [
            ConnectionDto::decl(),
            NoticeDto::decl(),
            ModelDto::decl(),
            SessionRowDto::decl(),
            ProjectStateDto::decl(),
            SkillDto::decl(),
            SideQuestionDto::decl(),
            crate::git::GitStatusDto::decl(),
            crate::git::PullRequestDto::decl(),
            TranscriptDto::decl(),
            ItemDto::decl(),
            ToolDto::decl(),
            DiffDto::decl(),
            ApprovalDto::decl(),
        ] {
            generated.push_str("export ");
            generated.push_str(&declaration);
            generated.push_str("\n\n");
        }
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/lib/generated.ts");
        if std::env::var_os("QQ_EXPORT_TYPES").is_some() {
            std::fs::write(&path, &generated).unwrap();
        }
        let checked_in = std::fs::read_to_string(&path)
            .expect("generate the TypeScript contract with QQ_EXPORT_TYPES=1");
        assert_eq!(
            checked_in.replace("\r\n", "\n"),
            generated,
            "view DTOs changed; regenerate the TypeScript contract"
        );
    }

    #[test]
    fn display_path_hides_only_the_verbatim_prefix() {
        assert_eq!(display_path(Path::new(r"\\?\C:\work\qq")), r"C:\work\qq");
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\host\share\qq")),
            r"\\host\share\qq"
        );
        assert_eq!(display_path(Path::new(r"C:\work\qq")), r"C:\work\qq");
        assert_eq!(display_name(Path::new(r"\\?\C:\work\qq")), "qq");
    }
}
