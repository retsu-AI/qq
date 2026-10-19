// Shared protocol and host types; native view DTOs are generated in generated.ts.
import type { WorktreeDto } from "./generated";
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
  /** Set when the folder is a linked Git worktree. */
  worktree: WorktreeDto | null;
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

export type {
  ConnectionDto as Connection,
  NoticeDto as Notice,
  ModelDto as Model,
  SessionRowDto as SessionRow,
  ProjectStateDto as ProjectState,
  TranscriptDto as Transcript,
  ItemDto as Item,
  ToolDto as Tool,
  ApprovalDto as Approval,
} from "./generated";
