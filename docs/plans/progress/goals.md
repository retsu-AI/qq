# Ledger — Goals

Plan: [`../goals.md`](../goals.md). One writer per file.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| G0 | The goal PR: protocol, store, goal in runs, `update_goal`, and autonomous-core AC4 | Planned | | | One PR, one protocol bump |
| G1 | — | Dropped (combined into G0) | | | No separate slice |
| G2 | The driver | Planned | | | Depends on G0; fresh goal recovery, not AC5 |
| G3 | Completion check | Planned | | | Depends on G2; owns full checked Goal 1 |
| G4 | Surfaces: `/goal`, `qq run --goal`, `qq goal` | Planned | | | Read-only panel after G0; execution after G2/G3 |
| G5 | Evidence | Planned | | | ENG-809 |

## Entries

### 2026-09-28 — plan opened

This is a design PR only. The reference survey is in
`../../research/goal-pursuit.md`. Of the reference harnesses,
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

### 2026-09-30 — #226 latest review closure (12 comments on `fb1e510`)

Worktree `.worktrees/pr-226-review`, branch `fix/eng-982-goal-review`, based
on PR head `5ebde18`. Design only; no Rust, wire, schema or runtime changed.
All 12 findings are valid at that head; the proposed ADR and plan now specify:
- explicit once/for-goal check consent, never a widened ordinary approval;
- one deadline for check exclusion/process; stale status/wait and results fenced;
- exact identity/revision CAS, immutable achieved goals, explicit stopped-goal resume;
- cancellation retains ownership; early resume rejects; archived spend remains charged;
- fresh goal recovery, not prompt-chain continuation; queued checks yield to prompts;
- final-slot handoff for prompt runs too; checked Goal 1 gate moves from G2 to G3;
- workflow-valid dropped statuses and synchronized dependencies/protocol shapes.
Two read-only source reviews confirmed approval, continuation, cancellation
and accounting contracts; final consistency review and verification follow.

### 2026-09-30 — final review and verification receipt for #226

- Verdict: **Approve for the proposed docs-only design** after independent recheck.
- Every spec edit invalidates its audit; neutral controls retag only valid audits.
- `cargo test -p qq docs_truth`: 5 passed in this worktree.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- Initial unrestricted workspace tests: 5 deadline failures; no Rust changed.
- `cargo test -p qq-core --lib sessions::tests::deadlines -- --test-threads=1`: 12 passed.
- `cargo test --workspace -- --test-threads=4`: passed; original timing failures disclosed.
- `cargo build --workspace`: passed. Cargo gates share `../../target`, not source changes.
- Full-PR relative links, fences, ledger status checks and `git diff --check`: passed.
- No runtime/hot-path changes; provider minimal profile and performance benches not applicable.
- ADR-0049 remains Proposed; lead decisions are in `decisions-needed.md` row 11.

### 2026-10-06 — delivery stack planned

- ENG-1011 tracks SQ0/SQ1/SQ2 before G0/G2/G3/G4/G5; no goal implementation claimed.
- Immediate-parent PR bases, retarget after merge; G0 retains AC4 and one bump.
- Side questions remain isolated from goal accounting and workspace check authority.
