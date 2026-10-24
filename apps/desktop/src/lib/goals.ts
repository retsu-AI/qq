// Session goals in the UI: labels for the server's goal snapshot, the budget
// presets offered when setting one, and the `/goal` composer commands. The
// server owns goal state; nothing here changes it.

import type { GoalSnapshot, GoalStatus, GoalNeedsUser } from "./types";
import type { GoalDefaultsDto } from "./generated";

export type BudgetPreset = "keep" | "defaults" | "focused" | "extended";

/** The goal revision the user saw; commands are refused if it changed. */
export interface SeenGoal {
  goal_id: string;
  revision: number;
}

export type GoalAction =
  | { type: "pause" }
  | { type: "resume" }
  | { type: "clear" }
  | { type: "approve_check"; scope: "once" | "for_goal"; command: string }
  | { type: "deny_check" };

export function seen(goal: GoalSnapshot): SeenGoal {
  return { goal_id: goal.goal_id, revision: goal.revision };
}

export type GoalTone = "active" | "attention" | "done" | "stopped";

/** A short status for the header chip and panel badge. */
export function goalStatus(goal: GoalSnapshot): { label: string; tone: GoalTone } {
  const status = goal.status;
  switch (status.type) {
    case "active":
      return { label: goal.pending_run_id ? "Working" : "Active", tone: "active" };
    case "paused":
      return status.reason.type === "user"
        ? { label: "Paused", tone: "stopped" }
        : { label: "Needs you", tone: "attention" };
    case "achieved":
      return { label: "Achieved", tone: "done" };
    case "blocked":
      return { label: "Blocked", tone: "attention" };
    case "budget_exhausted":
      return { label: "Out of budget", tone: "stopped" };
  }
}

/** Why the goal stopped, in a sentence, or null while it is active. */
export function goalReason(status: GoalStatus): string | null {
  switch (status.type) {
    case "active":
    case "achieved":
      return null;
    case "paused":
      return status.reason.type === "user"
        ? "Paused by you. Resume to continue."
        : needsUser(status.reason.cause);
    case "blocked": {
      const reason = status.reason;
      switch (reason.type) {
        case "model":
          return `The agent reported it is blocked: ${reason.reason}`;
        case "no_progress":
          return "Stopped: recent runs made no progress.";
        case "repeated_failure":
          return "Stopped: runs kept failing.";
        case "check_failing":
          return "Stopped: the completion check kept failing.";
        case "audit_exhausted":
          return "Stopped: the achievement audit did not pass in its allowed attempts.";
      }
      return "Blocked.";
    }
    case "budget_exhausted":
      return {
        runs: "Used every allowed run.",
        tokens: "Reached the token limit.",
        cost: "Reached the cost limit.",
        deadline: "Reached the deadline.",
        tokens_unknown: "Stopped: token use could not be measured against the limit.",
        cost_unknown: "Stopped: cost could not be measured against the limit.",
      }[status.reason];
  }
}

function needsUser(cause: GoalNeedsUser): string {
  switch (cause.type) {
    case "check_approval":
      return "Waiting for your consent to run the completion check.";
    case "check_interrupted":
      return "A completion check was interrupted; resolve its effects, acknowledge it, then resume.";
    case "approval":
      return "Waiting for you to answer a held tool call.";
    case "authentication":
      return `Sign-in needed: ${cause.message}`;
    case "configuration":
      return `Configuration needs review: ${cause.message}`;
    case "credentials":
      return `Credentials needed: ${cause.message}`;
  }
}

/** The check needs consent before the goal can verify itself. */
export function needsCheckConsent(goal: GoalSnapshot): boolean {
  return (
    goal.status.type === "paused" &&
    goal.status.reason.type === "needs_user" &&
    goal.status.reason.cause.type === "check_approval"
  );
}

/** "3 / 20 runs · 42m left" */
export function budgetSummary(goal: GoalSnapshot, now = Date.now()): string {
  const runs = `${goal.counters.runs} / ${goal.budget.max_goal_runs} runs`;
  const left = goal.budget.deadline_ms - now;
  return left > 0 ? `${runs} · ${duration(left)} left` : `${runs} · deadline passed`;
}

/** Whole minutes, hours or days: "42m", "5h 10m", "3d 4h". */
export function duration(ms: number): string {
  const minutes = Math.max(0, Math.round(ms / 60_000));
  if (minutes < 60) return `${minutes}m`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return minutes % 60 ? `${hours}h ${minutes % 60}m` : `${hours}h`;
  const days = Math.floor(hours / 24);
  return hours % 24 ? `${days}d ${hours % 24}h` : `${days}d`;
}

/** What a preset means for this workspace, for the dialog. */
export function presetLabel(preset: BudgetPreset, defaults: GoalDefaultsDto | null): string {
  if (preset === "keep") return "Keep the current deadline and limits";
  if (!defaults) return { defaults: "Configured defaults", focused: "Focused", extended: "Extended" }[preset];
  const [ms, runs] =
    preset === "defaults"
      ? [defaults.duration_ms, defaults.default_runs]
      : preset === "focused"
        ? [Math.min(defaults.duration_ms, 3_600_000), Math.min(defaults.default_runs, 20)]
        : [defaults.max_duration_ms, defaults.max_runs];
  const name = { defaults: "Configured defaults", focused: "Focused", extended: "Extended" }[preset];
  return `${name}: up to ${duration(Math.min(ms, defaults.max_duration_ms))} and ${Math.min(runs, defaults.max_runs)} runs`;
}

export type GoalCommand =
  | { kind: "set"; objective: string }
  | { kind: "show" }
  | { kind: "edit" }
  | { kind: "pause" }
  | { kind: "resume" }
  | { kind: "clear" };

/** `/goal …` typed in the composer, or null for anything else. */
export function parseGoalCommand(text: string): GoalCommand | null {
  const match = /^\/goal(?:\s+([\s\S]*))?$/.exec(text.trim());
  if (!match) return null;
  const rest = (match[1] ?? "").trim();
  if (!rest) return { kind: "show" };
  if (rest === "edit" || rest === "pause" || rest === "resume" || rest === "clear")
    return { kind: rest };
  return { kind: "set", objective: rest };
}
