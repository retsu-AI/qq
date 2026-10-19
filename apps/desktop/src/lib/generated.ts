// Generated from src-tauri/src/view.rs. Run QQ_EXPORT_TYPES=1 cargo test typescript_view_contract_is_current.
import type { SessionStatus, RunActivity, MessageState, ToolCallState, ApprovalMode, Group, RunOutcome, TokenUsage, ShellPreview, Question } from './types';

export type ConnectionDto = "starting" | "connecting" | "replaying" | "live" | "offline" | "failed";

export type NoticeDto = { level: "info" | "warning" | "error", text: string, };

export type ModelDto = { id: string, provider: string, model: string, name: string | null, context_window: number | null, reasoning_efforts: string[], };

export type SessionRowDto = { id: string, parent_id: string | null, depth: number, title: string, group: Group, status: SessionStatus, activity: RunActivity | null, active_tool: string | null, tail: string, unread: number, awaiting_approval: number, queued_prompts: number, model: string | null, updated_at_ms: number, failed: boolean, };

export type ProjectStateDto = { project_id: string, path: string, name: string, connection: ConnectionDto, error: string | null, owns_server: boolean, sessions: Array<SessionRowDto>, focused: string | null, transcript: TranscriptDto | null, models: Array<ModelDto>, selected_model: string | null, can_steer: boolean, can_interrupt: boolean, profiles: Array<string>, tool_hosts: Array<{ name: string; generation: number; tool_count: number; ready: boolean; message?: string }>, 
/**
 * The workspace's commands and skills, invoked as `/name`.
 */
skills: Array<SkillDto>, 
/**
 * The server answers side questions during a run.
 */
can_side_question: boolean, notice: NoticeDto | null, };

export type SkillDto = { name: string, kind: "command" | "skill", description: string, };

export type SideQuestionDto = { id: string, thread_id: string, question: string, answer: string, state: "running" | "completed" | "failed" | "cancelled" | "timed_out" | "interrupted", };

export type GitStatusDto = { 
/**
 * False when the project is not inside a Git work tree.
 */
repo: boolean, 
/**
 * Branch name; null when HEAD is detached.
 */
branch: string | null, upstream: string | null, ahead: number, behind: number, 
/**
 * Changed, staged and untracked paths.
 */
changed: number, conflicted: number, 
/**
 * The current branch's pull request, when `gh` is installed and finds one.
 */
pr: PullRequestDto | null, 
/**
 * `gh` is on PATH, so pull requests can be created.
 */
gh: boolean, };

export type PullRequestDto = { number: number, 
/**
 * `OPEN`, `CLOSED` or `MERGED`, as `gh` reports it.
 */
state: string, draft: boolean, title: string, url: string, };

export type WorktreeDto = { 
/**
 * Checked-out branch; null when detached.
 */
branch: string | null, 
/**
 * The main checkout this worktree belongs to.
 */
main_path: string, main_name: string, };

export type TranscriptDto = { session_id: string, title: string, loading: boolean, status: SessionStatus, activity: RunActivity | null, active_run_id: string | null, model: string | null, approval_mode: ApprovalMode, profile: string, reasoning_effort: string | null, approval_delegate: string | null, context_tokens: number | null, context_window: number | null, cost_usd_nanos: number | null, queued_prompts: number, items: Array<ItemDto>, 
/**
 * Oldest first; the server keeps a bounded recent window.
 */
side_questions: Array<SideQuestionDto>, };

export type ItemDto = { "kind": "user", id: string, text: string, state: MessageState, steering: boolean, } | { "kind": "assistant", id: string, text: string, refusal: string, state: MessageState, truncated: boolean, } | { "kind": "reasoning", id: string, text: string, streaming: boolean, } | { "kind": "tool" } & ToolDto | { "kind": "run_end", id: string, outcome: RunOutcome | null, duration_ms: number | null, usage: TokenUsage | null, cost_usd_nanos: number | null, tool_calls: number, };

export type ToolDto = { id: string, run_id: string, name: string, arguments: string, state: ToolCallState, result: string | null, is_error: boolean, diff: DiffDto | null, live_output: string | null, duration_ms: number | null, settled: string | null, approval: ApprovalDto | null, 
/**
 * The child session a `spawn_agent` call created, for linking.
 */
child_session: string | null, };

export type DiffDto = { path: string, diff: string, };

export type ApprovalDto = { shell: ShellPreview | null, edit: { path: string; diff: string } | null, question: { questions: Question[] } | null, fetch: { host: string; url: string; method?: string } | null, escalation: string | null, 
/**
 * Whether "for session"/"for workspace" grants can be offered.
 */
grantable: boolean, grant_label: string, };

