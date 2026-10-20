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

// Session goals (qq-protocol goals.rs). Tagged enums keep the protocol's
// `type` field; identifiers are strings.
export type GoalNeedsUser =
  | { type: "check_approval" }
  | { type: "check_interrupted" }
  | { type: "approval"; tool_call_id: string }
  | { type: "authentication"; message: string }
  | { type: "configuration"; message: string }
  | { type: "credentials"; message: string };

export type GoalStatus =
  | { type: "active" }
  | {
      type: "paused";
      reason: { type: "user" } | { type: "needs_user"; cause: GoalNeedsUser };
    }
  | { type: "achieved" }
  | {
      type: "blocked";
      reason:
        | { type: "model"; reason: string }
        | { type: "no_progress" }
        | { type: "repeated_failure" }
        | { type: "check_failing" }
        | { type: "audit_exhausted" };
    }
  | {
      type: "budget_exhausted";
      reason: "runs" | "tokens" | "cost" | "deadline" | "tokens_unknown" | "cost_unknown";
    };

export interface GoalSnapshot {
  session_id: string;
  goal_id: string;
  revision: number;
  objective: string;
  check: { command: string; timeout_ms: number } | null;
  check_authorization: "pending" | "once" | "consumed" | "for_goal" | null;
  budget: {
    max_goal_runs: number;
    deadline_ms: number;
    max_total_tokens?: number;
    max_cost_usd_nanos?: number;
  };
  checklist: Array<{
    id: number;
    text: string;
    state: "pending" | "in_progress" | "done" | "dropped";
  }>;
  notes: string;
  status: GoalStatus;
  counters: {
    runs: number;
    usage: TokenUsage | null;
    estimated_cost_usd_nanos: number | null;
    audits: number;
    check_failures: number;
    no_progress_streak: number;
    failure_streak: number;
  };
  next_run_at_ms: number;
  wait: { until_ms: number; reason: string } | null;
  achievement_claim?: { run_id: string; revision: number; evidence: string };
  audit_pending: boolean;
  audit_revision: number | null;
  pending_run_id: string | null;
  check_output: string | null;
}
