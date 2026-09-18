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

### J4/J5 and pending-state portion of J7 — 2026-09-18

Typed reviewer spend now charges the shared runtime budget and persists with the
verdict/run totals. Added finite request/repair limits, core timeout, response-byte
cap, criterion-specific decisions and conservative uncertainty handling. The
chunked-body regression failed before the cap and passed after it; parser tests
went red then green for criterion IDs and uncertainty. Completed-verdict accounting
passed independent read-only inspection.

That inspection confirmed pending cancellation accounting needed repair. Added
durable start markers for tool/final reviews, unknown accounting during dispatch,
and one unavailable unknown-spend receipt on cancellation. Public session
regression passes for both boundaries. Current checkpoint subset: 22 passed.
Protocol 24 and golden receipts are being qualified. No paid calls or real-model
speed/quality claims. Full workspace, current-head reviews, routing and passive
observer work remain outstanding.

### J2/J4/J5 independent review corrections — 2026-09-18

Removed the unnecessary fresh-tool requirement for final-answer corrections;
the finite two-correction limit remains. Root HTTP fixture now asserts the pinned
model, all three criterion distributions and final evidence payload. Parent-owned
child work retains reviewer identity and profile without a database migration;
user followups/publicly parented sessions resolve current user configuration.
Unknown inherited identities fail before credential lookup. Focused checks:
24 core checkpoint tests, five root/adapter tests, one child inheritance test pass.
Earlier full workspace run passed all targets except four NO_COLOR-sensitive TUI
tests; all 245 TUI tests passed with NO_COLOR removed. Final workspace rerun
started after inheritance corrections. Workspace all-feature Clippy passed before
these last corrections; final Clippy and performance comparison still pending.

### First stacked PR qualification — 2026-09-18

Full `env -u NO_COLOR cargo test --workspace --locked --offline --no-fail-fast`
passed; workspace all-target/all-feature Clippy, build and formatting passed.
Independent Spec and Standards reviews approve the corrected inheritance. Removed
two unused SQL columns; both reserved-reload regressions pass after that cleanup.
Root rejects the reserved routing opt-in instead of silently ignoring it. J6 and
passive advisory J7 remain separate planned slices, not shipped capabilities.

Thirty alternating release A/B pairs, then 30 A/A controls per fixture:
| Metric | Base median | Candidate median | Change | A/B p95 | A/A p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Disabled read-tool loop | 53,190 ns | 55,650.5 ns | +4.63% | 72,338 → 77,873 ns | 80,307 → 81,732 ns |
| Plan compilation | 25,251.5 ns | 25,230 ns | −0.09% | 26,114 → 25,909 ns | 26,184 → 25,897 ns |
| Descriptor digest | 2,495 ns | 2,458 ns | −1.48% | 2,591 → 2,550 ns | 2,546 → 2,570 ns |

Median overhead is within 5%; tool-loop sample p95 is +7.65% and is **not
qualified**. Host I/O pressure some avg10 was 18.31–26.35%; A/A tail change
was +1.77%, so it does not reproduce the same failure. A quiet-host tail run
remains necessary; no waiver or speed claim. These are per-process loop averages,
not individual-tool latency tails or full startup/fanout qualification.
Base is #72 dc59d14; no refreshed main arm or paid mode comparison performed.
Evidence: `target/qq-perf/jev-opt-in-2026-09-18/paired.json` and saved binaries.

#### J1–J5 final local receipt — 2026-09-18
Tests: final workspace **1,575 passed, 5 ignored**, no failures; NO_COLOR unset.
Commands: `cargo test --workspace --locked --offline --quiet`,
`cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`,
`cargo build --workspace`; all pass with the shared target directory.
Independent reviews: Spec and Standards approve; no outstanding code blockers.
Protocol 23→24 with matching golden fixtures; no database migration/dependency.
Open: hosted checks, quiet-host latency tails, paid evaluation, J6 routing and
passive J7 observer. No paid calls, merge, release or tracker update claimed.
