// Mirrors `src-tauri/src/view.rs` and the qq-protocol types it embeds.
// Field names are snake_case on purpose: they are the wire names.

export type Connection =
  | "starting"
  | "connecting"
  | "replaying"
  | "live"
  | "offline"
  | "failed";

export type SessionStatus = "idle" | "queued" | "running";
export type RunActivity =
  | "waiting_for_provider"
  | "reasoning"
  | "generating_response"
  | "preparing_tool_call";
export type MessageState =
  | "queued"
  | "streaming"
  | "complete"
  | "cancelled"
  | "failed"
  | "interrupted";
export type ToolCallState =
  | "requested"
  | "awaiting_approval"
  | "running"
  | "completed"
  | "failed"
  | "denied"
  | "interrupted";
export type ApprovalMode = "read_only" | "supervised" | "ask" | "auto" | "full";
export type Group = "needs_you" | "working" | "idle" | "done";

export interface ProjectInfo {
  id: string;
  path: string;
  name: string;
}

export interface Notice {
  level: "info" | "warning" | "error";
  text: string;
}

export interface Model {
  id: string;
  provider: string;
  model: string;
  name: string | null;
  context_window: number | null;
}

export interface SessionRow {
  id: string;
  parent_id: string | null;
  depth: number;
  title: string;
  group: Group;
  status: SessionStatus;
  activity: RunActivity | null;
  active_tool: string | null;
  tail: string;
  unread: number;
  awaiting_approval: number;
  queued_prompts: number;
  model: string | null;
  updated_at_ms: number;
  failed: boolean;
}

export interface ProjectState {
  project_id: string;
  path: string;
  name: string;
  connection: Connection;
  error: string | null;
  owns_server: boolean;
  sessions: SessionRow[];
  focused: string | null;
  transcript: Transcript | null;
  models: Model[];
  selected_model: string | null;
  can_steer: boolean;
  can_interrupt: boolean;
  notice: Notice | null;
}

export interface Transcript {
  session_id: string;
  title: string;
  loading: boolean;
  status: SessionStatus;
  activity: RunActivity | null;
  active_run_id: string | null;
  model: string | null;
  approval_mode: ApprovalMode;
  context_tokens: number | null;
  context_window: number | null;
  cost_usd_nanos: number | null;
  queued_prompts: number;
  items: Item[];
}

export interface TokenUsage {
  input_tokens: number;
  cache_read_input_tokens: number;
  cache_write_input_tokens: number;
  output_tokens: number;
  reasoning_tokens?: number;
}

/** Externally tagged on `type`; failure detail varies by kind. */
export type RunOutcome =
  | { type: "completed" }
  | { type: "cancelled" }
  | { type: "interrupted" }
  | { type: "failed"; failure: { kind?: string; message?: string } }
  | { type: "budget_exhausted"; exhaustion: { kind?: string } }
  | { type: "paused"; pause: { kind?: string; message?: string } };

export type Item =
  | {
      kind: "user";
      id: string;
      text: string;
      state: MessageState;
      steering: boolean;
    }
  | {
      kind: "assistant";
      id: string;
      text: string;
      refusal: string;
      state: MessageState;
      truncated: boolean;
    }
  | { kind: "reasoning"; id: string; text: string; streaming: boolean }
  | ({ kind: "tool" } & Tool)
  | {
      kind: "run_end";
      id: string;
      outcome: RunOutcome | null;
      duration_ms: number | null;
      usage: TokenUsage | null;
      cost_usd_nanos: number | null;
      tool_calls: number;
    };

export interface Tool {
  id: string;
  run_id: string;
  name: string;
  arguments: string;
  state: ToolCallState;
  result: string | null;
  is_error: boolean;
  diff: { path: string; diff: string } | null;
  live_output: string | null;
  duration_ms: number | null;
  settled: string | null;
  approval: Approval | null;
  child_session: string | null;
}

export interface ShellPreview {
  command: string;
  cwd?: string;
  verdict?: "allow" | "prompt" | "forbidden";
  reasons?: string[];
}

export interface Question {
  prompt: string;
  options: string[];
  free_text?: boolean;
}

export interface Approval {
  shell: ShellPreview | null;
  edit: { path: string; diff: string } | null;
  question: { questions: Question[] } | null;
  fetch: { url: string; host: string; method?: string } | null;
  escalation: string | null;
  grantable: boolean;
  grant_label: string;
}

export type ApprovalChoice =
  | { type: "once" }
  | { type: "session" }
  | { type: "workspace" }
  | { type: "deny" }
  | { type: "answer"; answers: string[] };

export interface Provider {
  id: string;
  /** Credential backend (`keyring`, `file`, …) when one is stored. */
  stored: string | null;
  /** Environment variable already supplying a key. */
  environment: string | null;
  accepts_key: boolean;
  browser_sign_in: boolean;
  recommended_model: string;
}

/** A project file whose QQ configuration waits for trust. */
export interface PendingFile {
  path: string;
  sections: string[];
  declarations: string[];
}

export interface BrowserSignIn {
  url: string;
  user_code: string | null;
}

export interface AuthChanged {
  provider: string;
  error: string | null;
}
