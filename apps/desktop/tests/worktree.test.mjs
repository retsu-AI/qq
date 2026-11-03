import { test } from "node:test";
import assert from "node:assert/strict";
import { branchFromTask } from "../src/lib/worktree.ts";

test("branch names come from the task's first meaningful words", () => {
  assert.equal(branchFromTask("Fix the login bug in @src/auth.rs"), "qq/fix-login-bug");
  assert.equal(
    branchFromTask("Please add retry backoff to the HTTP client, with jitter and caps"),
    "qq/add-retry-backoff-http-client",
  );
  assert.equal(branchFromTask("Größe ändern"), "qq/gr-e-ndern");
});

test("an empty task gets a timestamped name", () => {
  assert.equal(
    branchFromTask("  ", new Date(2026, 9, 19, 14, 5)),
    "qq/task-1019-1405",
  );
});
