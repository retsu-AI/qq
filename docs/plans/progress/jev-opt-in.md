# Ledger — optional Jev

Owner: this stacked implementation session. Base `dc59d14` / draft #72.

| Slice | Goal | Status | Branch/PR | Notes |
| --- | --- | --- | --- | --- |
| J1–J9 | Implement Jev review recommendations and qualify opt-in paths | In progress | `feat/eng-791-jev-opt-in`; PR pending | See owning plan for requirement-by-requirement acceptance |

## Entries

### 2026-09-18 — start

Fresh base `dc59d14`, main `c404ae5`; isolated worktree `/tmp/qq-jev-opt-in`.
Original main review docs preserved. User authorizes stacked implementation;
quick focused delivery, up-to-date docs and no self-merge.
Baseline tool_dispatch/plan_compile release benchmarks started before code edits;
raw evidence: `target/qq-perf/jev-opt-in-2026-09-18/baseline.log`.
Linear ENG-791 read still requires connector reauthentication; PR #72 is open/draft.
Public runtime/session, resolved config, HTTP and client boundaries are the
accepted review's test seams. Independent read-only maps cover config and routing.

### J1 queued steering — 2026-09-18

Red `dc84c96`: final checkpoint returned Supported then Completed without applying
queued input. Added a post-review steering boundary; same regression now passes.
Baseline release measurements: tool loop 61,241 ns; plan compile 25,670 ns;
descriptor digest 2,581 ns. Single recordings are baselines, not tail acceptance.
Interrupting review, effective task revision and durable review accounting remain
in progress. No paid calls or source changes outside the stacked worktree.

### J1 interrupt steering — 2026-09-18

Held-review regression failed with timeout before the fix. Reviewer now selects
interrupting steering and records Unavailable before applying the new input.
Checkpoint-focused suite: 17 passed, 0 failed. Queued and interrupt cases pass.
Design updated; J1 locally implemented, independent review remains required.

### J2/J3 local implementation — 2026-09-18

Default-off configuration, profiles, explicit disable, provenance and cache identity
implemented. Credential-present/off regression went red then green; 82 config
tests and four focused root Jev tests passed. Independent config review found
isolated TUI QA could accept enabled Jev; new regression reproduced it and the
fixture validator now rejects enabled capabilities before credential resolution.

Long-evidence selective review regression went red then green: eight executable
calls retain normal batching, one final assessment fits its bound, and omissions
are explicit. Removed the unbounded ineffective verdict cache. Review context now
includes continuation observations and applied steering; disabled runs allocate
no evidence projection. Checkpoint-focused suite: 17 passed at this intermediate
head. Full workspace and current-head independent review remain J8 gates.
Routing configuration is reserved for J6 and is not yet a router; do not expose
it as a shipping capability until dispatch and failure behavior are tested.

J2/J3 validation: config 82 passed; core 654 passed, 3 ignored; MCP integration
1 passed. Five loopback fetch fixtures were sandbox-denied on the first run; the
authorized rerun passed. Checkpoint-focused current run: 18 passed. Isolated QA
regression green. These are local checks, not hosted/workspace acceptance.
