# Ledger — Goals

Plan: [`../goals.md`](../goals.md). One writer per file.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| G0 | The goal PR: protocol, store, goal in runs, `update_goal`, and autonomous-core AC4 | Planned | | | One PR, one protocol bump |
| G1 | — | Merged into G0 | | | |
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

### 2026-09-28 — Codex review on #226 (17 comments on `ccc8b47`)

All 17 were checked, and none were file-list comments. All 17 are fixed.
- The design faults:
  - the check takes a workspace-wide exclusion, since other sessions can
    run in the same workspace (`tools.md:1440–1442`);
  - the check is authorized by an exact-command held approval (a server
    command has no `--allow-shell` caller, `src/cli.rs:207`);
  - pause, replace and clear cancel queued work and ask running work to
    cancel;
  - `update_goal` is dispatched serially (`lib.rs:3372–3396`);
  - the approval wait is clamped to the deadline, and its pause takes
    precedence;
  - stale check results are ignored;
  - a check interrupted by a crash pauses the goal instead of re-running;
  - `session_goals` keeps history rows;
  - the last run under the cap gets a wrap-up;
  - runs with no committed turn don't count toward the cap, so an outage
    only uses up the deadline;
  - a user prompt cancels an unclaimed goal run;
  - `qq run --goal` returns on any non-`active` state;
  - managed ceilings can only be lowered;
  - G0 and G1 merged into one goal PR that includes AC4;
  - the stale AC7–AC9 ledger rows and the § 4 → § 8 references are fixed.
- None were declined.
