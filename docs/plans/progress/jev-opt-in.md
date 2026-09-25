# Ledger — optional Jev

Owner: this stacked implementation session. Base `dc59d14` / draft #72.

| Slice | Goal | Status | Branch/PR | Notes |
| --- | --- | --- | --- | --- |
| J1–J5 + visible J7 | Optional review, correctness and receipts | In review | [#74](https://github.com/retsu-AI/qq/pull/74), stacked on #72 | Local and hosted checks green; quiet-host tails open |
| J6a | Explicit model effort | In review | [#76](https://github.com/retsu-AI/qq/pull/76), stacked on #74 | Local checks/reviews green; performance gate unresolved; hosted CI 35394821612 successful |
| J6b | Optional routing | In review | [#77](https://github.com/retsu-AI/qq/pull/77), stacked on #76 | Hosted CI 35406214280 successful; incremental median within budget; full-stack qualification pending |
| Passive J7 | Advisory observer | In review | [#78](https://github.com/retsu-AI/qq/pull/78), stacked on #77 | Local workspace gates, independent review and hosted CI 35407913885 passed |
| J8–J9 | Qualification and delivery | In progress | #74 | Live evaluation and remaining slices open |

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

### Stacked delivery — 2026-09-18

Pushed `1d2b3604ffd1393a285cc2070799c88df7d2b26d`; opened draft
[#74](https://github.com/retsu-AI/qq/pull/74), base `feat/jev-runtime-checkpoints`
(#72, dc59d14). GitHub connector creation returned 403; authenticated GitHub CLI
created the authorized PR. Local workspace is clean after the implementation
commit. Linear connector remains unavailable; issue state was not changed.
No merge or release. Broader routing/advisory work remains planned.

### J6a explicit effort foundation — 2026-09-18

Follow-up branch `feat/eng-791-jev-routing` starts from #74 head 1405ff0.
Scope: explicit effort in trusted config/profiles, request dispatch and immutable
plan identity. Routing will preserve this pinned choice. Default omission remains
unchanged; no Jev activation is coupled to effort. Prior candidate benchmark
binaries are the pre-change baseline for this slice. No new dependency or runtime.

J6a tests went red for missing effort config/runtime methods, then green.
Config/profile precedence, explicit none versus omission, workspace re-trust,
cache isolation and provider dispatch are covered. Independent Spec/Standards
reviewers approve this bounded slice. The root suite hit an existing one-second
silent-turn timeout under load (156 passed, one failed, one ignored); the exact
regression passed alone in 0.19 s. No timeout was widened. Final suite pending.
PR #74 hosted CI run 35392565866 completed successfully at 1405ff0.

#### J6a local receipt — 2026-09-18
Workspace tests: 1,579 passed, five ignored, no failures, NO_COLOR unset.
`cargo fmt --all -- --check`, strict workspace all-target/all-feature Clippy,
and workspace build pass (locked/offline, shared target directory).
Core regression observes the same effort on both sides of a real tool turn.
Independent Spec/Standards: approve. Descriptor 7→8; ADR-0031 and runbook updated.
No new dependency, migration or Jev inference. This commit is local to the
routing follow-up branch; #74 remains at its independently green head.
J6b selection/admission/accounting and J7 passive observer remain unfinished.

### J6a delivery qualification — 2026-09-18

Rechecked #74: open/draft, head 1405ff0, hosted CI 35392565866 succeeded.
Explicit effort remains the bounded follow-up; automatic routing and passive
observation are not represented as implemented. Release A/B and A/A use the
retained #74 candidate binaries as baseline and ecc6a1f as candidate.
Raw evidence: `target/qq-perf/jev-effort-2026-09-18/paired.json`.

#### J6a performance receipt — 2026-09-18
Thirty release A/B pairs against #74, followed by 30 same-binary A/A pairs.
Tool loop median: 52,282.5 → 55,146 ns (+5.48%); sample p95 71,741 → 85,339 ns.
A/A median: 54,681.5 → 51,717.5 ns (−5.42%); p95 82,130 → 80,980 ns.
Plan compile median: 24,729.5 → 24,583.5 ns (−0.59%).
Descriptor digest median: 2,433.5 → 2,343.5 ns (−3.70%).
Host I/O some avg10: 31.38–47.17%. No overlapping root build/test during samples.
The tool-loop 5% gate is not met; noisy control does not waive it. Quiet-host
measurement remains required before merge qualification. This draft delivers
reviewable functionality, not a performance or Jev acceleration claim.

### J6a stacked delivery — 2026-09-18

Pushed c5feef4 and opened draft [#76](https://github.com/retsu-AI/qq/pull/76),
base `feat/eng-791-jev-opt-in` (#74, 1405ff0). The GitHub connector still cannot
write PR metadata (403); authenticated CLI performed the authorized draft/update.
#74 body now accurately records successful hosted CI. #76 does not claim its
performance gate, automatic routing or advisory observation are complete.
No merge, paid evaluation or Linear update. Broader goal remains in progress.

### J6b durable routing start — 2026-09-18

Branch `feat/eng-791-jev-routing-accounting` starts from #76 head 2a56015.
First regression covers known/unknown routing spend through cancellation and
restart, duplicate dispatch and late responses. Routing occurs before ordinary
plan preparation; its receipt must survive failure and seed budgets once.
The #76 release binaries remain the pre-change baseline. No paid calls.

J6b focused evidence: four routing tests pass (selection/fallback, budget charge,
pending cancellation, restart/late-response guards and context observation).
Independent accounting review found routing-only settlement cleared the main
context meter; separate spend/turn flags repair it. New schema migration keeps
old runs unrouted and ordinary queued recovery unchanged. Protocol 25 fixtures
include routing events; production routing remains unavailable pending adapter,
pin provenance, inherited opt-in and direct-CLI integration. Work is uncommitted.

J6b core suite after integration: 667 passed, three ignored, with loopback
fixtures authorized outside the sandbox. Standards review approves the current
core seam; production activation remains outside that verdict. Follow-up repair
persists known routing spend before selected-provider loading. Four focused
routing tests include pending known-spend cancellation/recovery, and workspace
verification is running on that final repair. No change pushed in this slice.

#### J6b core preparation receipt — 2026-09-18
Workspace: 1,584 passed, five ignored after accounting/protocol repairs.
Final schema validation addition: migration suite 44 passed, one ignored.
Strict workspace all-target/all-feature Clippy, formatting and workspace build pass.
Independent accounting/Standards reviews approve the core seam and corrections.
Schema 30→31; protocol 24→25 with retained old and new wire/headless fixtures.
ADR-0032, architecture/protocol docs and ledger updated; no dependency added.
Known routing spend persists before provider reload; main context meter preserved.
Unfinished: production Jev adapter, model-pin provenance, inherited opt-in,
direct ask routing, passive advisory, performance/live qualification.
This is local implementation progress, not a shipping routing capability or PR.

### J6b model-choice provenance — 2026-09-18

Schema 32 and protocol-25 selections retain configured fallback versus explicit
pin intent across commands, reservation, reconnect and child creation. Root
loading resolves fallbacks from configuration; routing cannot replace a pin.
Added legacy wire/migration, pin rejection and root-resolution regressions.
Independent read-only spec/standards reviews approve the bounded change, with
requested pin/loader coverage now added. Production adapter remains unfinished.
No paid inference, new dependency, push or shipping claim in this session.

J6b provenance receipt: workspace 1,590 passed, five ignored; formatting,
strict all-target/all-feature Clippy and workspace build pass. Pin rejection
retains the chosen provider and charges auxiliary spend once. Root loader tests
cover a conflicting configured default; migration and legacy-wire tests preserve
pins. ADR-0033 and architecture/protocol/plan docs updated. Protocol 25 is still
unpublished and includes both the routing and provenance changes. Performance
qualification remains open; no speed claim or additional hosted PR yet.

### J6b concrete adapter and activation — 2026-09-18

Connected TypeSafe task selection to compiled plans, sessions and direct ask.
Owned children inherit routing identity/off before credentials; user followups
reload current settings. Added declared model effort capabilities and bounded
candidate fingerprinting after independent review found stale-cache and unknown
model-effort risks. Embedded compilation now preserves the router too.
HTTP contract tests cover masking, bounds, invalid/uncertain choices and spend;
cache/pin/inheritance regressions are running. ADR-0034 and runbook updated.
No paid calls or new dependency. Performance and live qualification remain open.

J6b verification: 1,597 workspace tests passed, five ignored. Independent
Spec/Standards rechecks approve candidate identity, declared effort support and
embedded router preservation. Strict workspace Clippy passes after boxing the
configuration-only model patch and moving the masking re-export before tests.
Application/configuration tests are rerunning on that representation change.
Refreshed #76: open/draft, head 2a56015, hosted CI 35394821612 successful.

Final representation check: 249 application/config tests passed, one ignored;
workspace build and formatting pass. No real inference credentials or live APIs used.
Descriptor 8→9 golden encoding and routing candidate fingerprints are covered.
The retained #76 release binaries are the off-path comparison baseline.

#### J6b performance receipt — 2026-09-18
Candidate cf61126 versus retained #76 binaries; 30 alternating A/B and 30 A/A
pairs per fixture, with no overlapping build/test during measurements.
Tool-loop median 54,392 → 56,136.5 ns (+3.21%); p95 70,945 → 70,783 ns.
A/A tool median +1.23%; p95 75,752 → 69,457 ns.
Plan compile median 24,977.5 → 24,715.5 ns (−1.05%); digest +0.69%.
I/O some avg10 sampled 18.03–22.40%. Raw evidence:
`target/qq-perf/jev-routing-2026-09-18/paired.json` (untracked).
This increment is within the 5% median budget. The earlier #76 gate and
quiet-host full-stack qualification remain unresolved; no Jev speed claim.

### J6b stacked delivery — 2026-09-18

Pushed b7a537a and opened draft [#77](https://github.com/retsu-AI/qq/pull/77),
base `feat/eng-791-jev-routing` (#76). GitHub connector creation still returned
403; authenticated CLI created the authorized draft. No merge or paid inference.
Remaining full-goal work: passive advisory observation, hosted checks and
full-stack performance/live qualification. Linear reauthentication remains open.

### Passive J7 verification — 2026-09-18

Branch `feat/eng-791-jev-advisory` adds explicit `qq jev observe` with finite
external budgets, bounded run-specific evidence and a synced JSONL journal.
Five focused advisory tests pass, including a real local server demonstration
that a held assessment does not delay the next run and cancellation preserves
the pending dispatch. No paid inference. Runbook updated with restart and
unknown-spend behavior. Workspace gates, independent review and publication
remain outstanding. PR #77 CI 35406214280 completed successfully.

Full workspace tests now pass: 1,602 passed, five ignored, no failures.
`cargo fmt --all -- --check`, workspace Clippy with all targets/features and
`-D warnings`, and `git diff --check` pass. Independent review requested;
workspace build and publication remain in progress.

Workspace build passed. Independent Standards review approved the passive
observer with no concrete blockers; documented evidence-window and pending-spend
limitations remain. No protocol, schema, runtime hot-path or dependency change
in this slice. Full-stack performance qualification is still outstanding.

### Passive J7 stacked delivery — 2026-09-18

Published d405afb as draft [#78](https://github.com/retsu-AI/qq/pull/78),
verified base `feat/eng-791-jev-routing-accounting` and matching remote head.
Hosted CI 35407824495 started. Retained release fixtures compare original
dc59d14 against the current core implementation, unchanged by passive J7;
30 alternating pairs and same-binary controls run under
`target/qq-perf/jev-full-stack-2026-09-18/`. This comparison does not measure
CLI startup or live inference. No merge or paid requests.

### Full repair-stack focused performance — 2026-09-18

Retained dc59d14/cf61126 release fixtures, 30 alternating A/B and 30 A/A pairs
per fixture; verified no core/config/provider/manifest/lock changes between
cf61126 and d405afb. Tool-loop median 53,166→53,906 ns (+1.39%); p95
70,438→70,447 ns. Same-binary median +2.68%, p95 73,641→80,467 ns (+9.27%).
Plan compilation median 25,474→24,793.5 ns (-2.67%); digest -6.00%.
Raw receipt: `target/qq-perf/jev-full-stack-2026-09-18/paired.json`.
The measured repair-stack median fits 5%, but I/O pressure 20.78–39.18% and
the failing same-binary tail prevent quiet-host tail qualification. This is
relative to PR #72, not main, and does not qualify absolute budgets, startup,
fanout or live Jev quality/savings. Those acceptance limits remain open.

### Complete candidate recording and hosted checks — 2026-09-18

PR #78 head 8264d77 passed hosted CI 35407913885 (Linux checks, wasm client,
native Windows teardown). Full H0 recording at that clean head completed with
77 metrics and all 25 correctness checks passing, 100 requested samples and
10 warmups. Receipt: `target/qq-perf/jev-full-stack-2026-09-18/candidate-h0.json`.
Default binary 48,725,376 bytes exceeds 48,000,000; minimal 42,079,152 exceeds
41,000,000. These are the two observed absolute-budget failures. Matching
main c404ae5 recording is running in `/tmp/qq-jev-main-perf`; no relative
conclusion yet. Budgets have not been changed or waived.

Landing requires incorporating the follow-up fixes before the original #72
reaches main: #72's old head still fails Clippy and a Windows teardown test,
while #74/#76/#77/#78 pass. Maintainers can merge the reviewed stack downward
(#78 into #77, then #77 into #76, #76 into #74, #74 into #72), preserving
ancestry and rerunning the resulting #72 checks before merging to main.
No PR has been merged by this session. Live Jev evaluation remains required
for speed/quality claims; this implementation makes none.

### Main comparison — 2026-09-18

Clean main c404ae5 full H0 recording completed with 77 metrics. Candidate
8264d77 versus main: default binary 48,090,784→48,725,376 bytes (+1.32%);
minimal 41,441,744→42,079,152 (+1.54%). Both absolute size failures predate
the stack. Startup medians improve: version -2.98%, server readiness -6.05%.
The budget checker exits 1: two absolute size and eight relative p95 failures
(idle shutdown, HTTP replay, eight-subscriber fanout, long shell, eight-stream
control/cancellation/output gap, restart replay). Same-binary full main control
is running; do not classify those tails as noise or waive them yet.
Reports: `/tmp/qq-jev-main-perf/target/qq-perf/jev-full-stack-2026-09-18/main-h0.json`
and candidate directory above; checker output retained as `main-comparison.txt`.

Full same-binary control completed (`main-aa-h0.json` in the baseline directory).
It reproduces the absolute size failures and HTTP replay, eight-subscriber
fanout and restart-replay p95 failures; other control failures include cursor
replay, fanout command acknowledgement and restart snapshot reconstruction.
It does not reproduce candidate idle-shutdown, long-shell or eight-stream
control/cancellation/output-gap failures. Those five candidate failures remain
unresolved and require focused paired measurement, not a blanket noise waiver.
Latest documentation head d336048 passed hosted CI 35408821021.

### Focused follow-up and qualification boundary — 2026-09-18

Thirty alternating main/candidate pairs plus thirty same-binary pairs per
eight-stream/shell fixture completed. Candidate control/cancellation/output-gap
p95 were 36.82/45.23/42.00 ms versus main 32.25/42.10/40.00 ms; each passes
its 20% relative and absolute fixture limits. Shell p95 110.54 versus 137.23 ms
also passes. Relevant medians differ by at most 0.39%. I/O pressure ranged
5.66–33.01%. Raw pairs: `r4-paired.json` in the candidate receipt directory.

The full candidate repeat at dbb3da8 passes idle shutdown: median 73,358 ns,
p95 106,931 ns versus main 75,282/111,810. Report `candidate-repeat-h0.json`
retains all 77 metrics. Its overall budget check still fails: both inherited
size limits and a different set of startup/replay/stream/load metrics, including
100-session throughput. Do not discard either recording or claim qualification.
The prior five unmatched failures are not stable across these measurements;
quiet-host full-suite acceptance remains unresolved. Stop shared-host reruns
here; a quiet host or explicit lead decision is needed to close this gate.
No source change follows the independently reviewed implementation. Latest
hosted CI 35409246345 passed at dbb3da8. Live Jev quality/savings remain unclaimed.
