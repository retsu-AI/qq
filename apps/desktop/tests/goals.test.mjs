import { test } from "node:test";
import assert from "node:assert/strict";
import {
  budgetSummary,
  duration,
  goalReason,
  goalStatus,
  needsCheckConsent,
  parseGoalCommand,
  presetLabel,
} from "../src/lib/goals.ts";

const goal = (status, extra = {}) => ({
  session_id: "s",
  goal_id: "g",
  revision: 3,
  objective: "Make the tests pass",
  check: { command: "cargo test", timeout_ms: 600000 },
  check_authorization: "pending",
  budget: { max_goal_runs: 20, deadline_ms: 10_000_000 },
  checklist: [],
  notes: "",
  status,
  counters: {
    runs: 3,
    usage: null,
    estimated_cost_usd_nanos: null,
    audits: 0,
    check_failures: 0,
    no_progress_streak: 0,
    failure_streak: 0,
  },
  next_run_at_ms: 0,
  wait: null,
  audit_pending: false,
  audit_revision: null,
  pending_run_id: null,
  check_output: null,
  ...extra,
});

test("status labels and tones follow the goal state", () => {
  assert.deepEqual(goalStatus(goal({ type: "active" })), { label: "Active", tone: "active" });
  assert.deepEqual(goalStatus(goal({ type: "active" }, { pending_run_id: "r" })).label, "Working");
  const consent = goal({
    type: "paused",
    reason: { type: "needs_user", cause: { type: "check_approval" } },
  });
  assert.deepEqual(goalStatus(consent), { label: "Needs you", tone: "attention" });
  assert.equal(needsCheckConsent(consent), true);
  assert.equal(needsCheckConsent(goal({ type: "paused", reason: { type: "user" } })), false);
  assert.equal(goalStatus(goal({ type: "achieved" })).tone, "done");
  assert.equal(
    goalStatus(goal({ type: "budget_exhausted", reason: "deadline" })).label,
    "Out of budget",
  );
});

test("reasons explain why a goal stopped", () => {
  assert.equal(goalReason({ type: "active" }), null);
  assert.match(goalReason({ type: "paused", reason: { type: "user" } }), /Resume/);
  assert.match(
    goalReason({ type: "blocked", reason: { type: "model", reason: "needs an API key" } }),
    /needs an API key/,
  );
  assert.equal(goalReason({ type: "budget_exhausted", reason: "runs" }), "Used every allowed run.");
  assert.match(
    goalReason({
      type: "paused",
      reason: { type: "needs_user", cause: { type: "credentials", message: "sign in to xai" } },
    }),
    /sign in to xai/,
  );
});

test("budgets and durations read naturally", () => {
  assert.equal(duration(42 * 60_000), "42m");
  assert.equal(duration(5 * 3_600_000 + 10 * 60_000), "5h 10m");
  assert.equal(duration(3 * 86_400_000 + 4 * 3_600_000), "3d 4h");
  assert.equal(duration(86_400_000), "1d");
  assert.equal(budgetSummary(goal({ type: "active" }), 10_000_000 - 42 * 60_000), "3 / 20 runs · 42m left");
  assert.equal(budgetSummary(goal({ type: "active" }), 20_000_000), "3 / 20 runs · deadline passed");
  const defaults = {
    duration_ms: 86_400_000,
    max_duration_ms: 604_800_000,
    default_runs: 200,
    max_runs: 2000,
    check_timeout_ms: 600_000,
  };
  assert.equal(presetLabel("focused", defaults), "Focused: up to 1h and 20 runs");
  assert.equal(presetLabel("extended", defaults), "Extended: up to 7d and 2000 runs");
  assert.equal(presetLabel("keep", defaults), "Keep the current deadline and limits");
});

test("/goal commands parse; other text does not", () => {
  assert.deepEqual(parseGoalCommand("/goal"), { kind: "show" });
  assert.deepEqual(parseGoalCommand("/goal pause"), { kind: "pause" });
  assert.deepEqual(parseGoalCommand("/goal edit"), { kind: "edit" });
  assert.deepEqual(parseGoalCommand("/goal  Make CI green\nand fast "), {
    kind: "set",
    objective: "Make CI green\nand fast",
  });
  assert.equal(parseGoalCommand("/goals"), null);
  assert.equal(parseGoalCommand("set a /goal"), null);
});
