# Ledger — Goals

Plan: [`../goals.md`](../goals.md). One writer per file.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| G0 | Protocol and store | Planned | | | Goal PR, with G1 |
| G1 | The goal in runs, `update_goal` | Planned | | | |
| G2 | The driver | Planned | | | Depends on autonomous-core AC5 |
| G3 | Completion check | Planned | | | |
| G4 | Surfaces: `/goal`, `qq run --goal`, `qq goal` | Planned | | | |
| G5 | Evidence | Planned | | | ENG-809 |

## Entries

### 2026-09-28 — plan opened

This is a design PR only. The reference survey is in
`design/goal-reference-survey-2026-09-28.md`. Of the reference harnesses,
only Codex has a real goal. OpenCode, Pi and fx have pieces: a todo list
that is never re-injected, a finish-turn hook, and a stop hook that fails
open.

ADR-0049 was still Proposed, so it was revised in place instead of
superseded. The first draft's run-chain goal became a session goal that
only a client can create. The revision adds:
- a runtime-owned goal driver;
- goal budgets enforced through `RunLimits`;
- an optional check command that the runtime runs;
- durable waits and backoff;
- pause-on-cancel as a runtime rule.

The loop guard in § 8 is unchanged. Autonomous-core AC7–AC9 move to this
plan as G0–G3.

### 2026-09-28 — self-review before opening the PR

Before opening, an independent read-only review checked the design against
source and found 12 problems. All were fixed:
- **The check is a `GoalCheck` run.** It holds the active slot, can be
  cancelled, uses a dedicated runner (the shell tool caps timeouts at
  600 s, `tools/shell.rs:23`), and makes no sandbox claim.
- **Unanswered approvals pause the goal** after `approval_wait` instead of
  using up the deadline.
- **Goal accounting lives inside `settle_run`,** so every settlement path
  charges it, including recovery (`settlement.rs:1061–1107`).
- **`AutoContinue` skips goal runs,** and goal successors take the goal
  clamp and the goal block as their first message.
- **A `NoProgress` pause gets a fresh run,** never a continuation.
- **Limits are clamped at start,** because they are fixed at insert today
  (`commands.rs:598–600`). User prompts are clamped too.
- **Audit rules:** only goal-origin runs can claim a status, and an
  exhausted audit allowance blocks the goal.
- **Stale claims** are rejected or merged, and an interrupting prompt does
  not pause the goal.
- **Descriptor and prefix claims** were restated honestly.
- **The size bound** now includes the check tail (16 KiB).
