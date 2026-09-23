# Token efficiency: more verified work per token

**Status:** TE0 documentation in review; TE1–TE8 planned, no new runtime
behavior or savings established. [Ledger](progress/token-efficiency.md).
**Baseline:** `8089a0e`. **Tracker:** [ENG-886](https://linear.app/retsu-ai/issue/ENG-886), project `qq`,
team `ENG`. Planning PR: [#141](https://github.com/retsu-AI/qq/pull/141).
**Research:** [current mechanisms and hypotheses](../design/token-efficiency.md).
**Decision:** [ADR-0042 (Proposed)](../adr/0042-verified-task-efficiency.md).

## Goal and non-goals

Reduce total tokens and dollars per independently verified completed root task,
with explicit success-rate and latency constraints. Include unsuccessful work
in the numerator: report total suite spend / verified successes, not just the
mean cost of successful runs. Zero successes means undefined/infinite cost per
success, never a zero-cost win. Report success rate and total spend alongside it.

Do not trade away approval, containment, durability, verification or explicit
model choices. No blanket cheaper-model routing, recursive-agent default,
semantic-memory platform, new execution framework, hidden-reasoning storage,
lossy history deletion, or paid evaluation is authorized by this plan PR.

## Existing owners: reuse rather than duplicate

| Concern | Existing owner | Relationship |
| --- | --- | --- |
| Paid evaluation budget and scheduling | [ENG-809](https://linear.app/retsu-ai/issue/ENG-809) | All live trials require its explicit spend/credential approval |
| D6b delegation arms and default decisions | [ENG-812](https://linear.app/retsu-ai/issue/ENG-812), [delegation plan](supervised-delegation.md) | TE3 consumes results; does not recreate this work |
| T13 tool ablations | [ENG-813](https://linear.app/retsu-ai/issue/ENG-813), [tool-layer plan](tool-layer.md) | Reuse fixtures/reports for TE1, TE2 and TE4 |
| RR10 tolerant argument decode | [ENG-872](https://linear.app/retsu-ai/issue/ENG-872), [reliability plan](run-reliability.md) | TE2 owns only additional schema ergonomics after reconciliation |
| Cache determinism and affinity controls | [ENG-833](https://linear.app/retsu-ai/issue/ENG-833), [ENG-800](https://linear.app/retsu-ai/issue/ENG-800) | TE7 synthesizes their evidence; no duplicate cache implementation |
| T14 tool selection | [ENG-818](https://linear.app/retsu-ai/issue/ENG-818) | TE7 compares stable exposure against progressive selection |
| Live context and summary quality | [ENG-810](https://linear.app/retsu-ai/issue/ENG-810), [ENG-807](https://linear.app/retsu-ai/issue/ENG-807) | TE6 requires retention evidence, not just shrinkage |
| Replay projection and context estimate | [ENG-804](https://linear.app/retsu-ai/issue/ENG-804), [ENG-869](https://linear.app/retsu-ai/issue/ENG-869) | Recheck merged state before touching context; do not redo their fixes |

## Task index

Each row is a PR-sized bounded deliverable. Split an implementation that exceeds
one PR as TE<n>.1 etc. in the ledger before starting; no parent slice licenses
an unbounded rewrite. All production changes amend their as-built design docs.

| ID | Deliverable | Inputs | Owned paths / docs | Acceptance |
| --- | --- | --- | --- | --- |
| TE0 | Baseline, roadmap, proposed decision, tracker links | Source/tracker inventory | This plan, design inventory, ADR-0042, indexes, ledger | Links, ownership, scope and tracker reviewed; no implementation claims |
| TE1 | Task-tree efficiency report and offline fixtures | TE0 | `xtask/src/eval.rs`, existing accounting under `crates/qq-core/src/`, `benchmarks/arms/`; design inventory | Deduplicated usage/outcomes, unknowns and category overlap tested; fixed-suite baseline |
| TE2 | Model-facing tool-schema ergonomics | TE1; RR10 ownership reconciliation | `crates/qq-core/src/tools/`, schema assembly in `src/`; `design/tools.md` | Reproduced contract failures, valid-call compatibility, no weakened mutation/approval validation; paired invalid-call rate |
| TE3 | Bounded discovery briefs and task-shaped delegation guidance | TE1 and D6b/ENG-812 receipt | Existing delegation prompts/config in `src/`, `crates/qq-core/src/sessions/subagents.rs`, `benchmarks/arms/`; delegation docs | Brief/result bounds and no recursion escalation; per-class economics measured before default changes |
| TE4 | One compact diagnostic-output path | TE1 and T13 fixture availability | Existing tool output handling in `crates/qq-core/src/tools/`; `design/tools.md` | One measured test/build format, bounded parser, exit/status fidelity, raw fallback and exact output retrieval |
| TE5 | Scoped evidence reuse experiment | TE1; accepted authority/retention contract before code | Existing spills/attachments/delegation in `crates/qq-core/src/`; tools/transcript design | One workspace-read evidence kind, stale/unauthorized/evicted cases, hard bounds and duplicate-read comparison |
| TE6 | Obsolete-evidence context projection experiment | TE1, TE4, ENG-804 state verified; ENG-807 gate for promotion | `crates/qq-core/src/sessions/{context,transcript,compaction}.rs`; transcript design | Existing stubbing baseline, deterministic replay, obligation retention, cache-aware comparison; opt-in only until qualified |
| TE7 | Cache and tool-exposure joint comparison | TE1; ENG-833/800/818 receipts | `benchmarks/arms/`, report fixtures; providers/tools design only for shipped changes | Cold/warm and compaction comparisons, missing-tool recovery, logical versus billed input; decision receipt without duplicate mechanisms |
| TE8 | One deterministic verification workflow | TE1 and TE4 | Existing `.qq/skills/`, `xtask/` or current tool path selected by traces; relevant design/runbook | One repeated workflow replaces measured turns, preserves approval/cancellation and truthful verification receipts |

Owned path families are candidate locations, not permission to edit everything
under them. Slice dispatch must resolve concrete files after reading scoped
instructions; shared manifests/protocol/store changes go through root.

## Linear slice map

| Slice | Issue |
| --- | --- |
| TE0 | [ENG-887](https://linear.app/retsu-ai/issue/ENG-887) |
| TE1 | [ENG-888](https://linear.app/retsu-ai/issue/ENG-888) |
| TE2 | [ENG-889](https://linear.app/retsu-ai/issue/ENG-889) |
| TE3 | [ENG-890](https://linear.app/retsu-ai/issue/ENG-890) |
| TE4 | [ENG-891](https://linear.app/retsu-ai/issue/ENG-891) |
| TE5 | [ENG-892](https://linear.app/retsu-ai/issue/ENG-892) |
| TE6 | [ENG-893](https://linear.app/retsu-ai/issue/ENG-893) |
| TE7 | [ENG-894](https://linear.app/retsu-ai/issue/ENG-894) |
| TE8 | [ENG-895](https://linear.app/retsu-ai/issue/ENG-895) |

Existing issues in the ownership table remain authoritative for their work;
these children track only the new deliverables and integration decisions.

## TE1 — measurement contract

Start from existing evaluation rows and usage accounting. Avoid instrumenting
full prompt bodies or introducing a telemetry service. Produce a versioned report
format with root task identity, attempt/run lineage, agent role, selected
model/effort, arm/config fingerprint, source/task revisions, outcome and verifier.

- Include independent success-verifier tokens, dollars and elapsed time in the
  primary task total, even when verification runs externally. Report execution
  and verification latency separately as well as end-to-end. An execution-time
  reviewer also serving as verifier is counted once. Verifier failure, timeout
  or disagreement is unresolved, not a success; retain its spend and report the
  unresolved count. Predetermine adjudication and bounded retry policy. Unknown
  verifier usage remains unknown and prevents a complete-cost savings claim.
- Count every unique request/attempt once across root, child, reviewer and
  compactor totals. Do not add an inclusive parent total to its children.
- Preserve reported input/cache-write/cache-read/output/reasoning categories and
  adapter semantics; mark unavailable categories/prices and usage lost on failed
  streams as unknown. No fabricated zeroes or double-counted reasoning.
- Report total attempted spend, tokens/success, dollars/success, success rate,
  p50/p95 latency, child count, invalid calls and recovery work. External advisory
  observers have a separately labelled optional combined total.
- Estimate instructions/schema/history/tool contribution separately from billed
  usage. Record estimator/version; do not describe byte estimates as exact tokens.
- Fingerprint repeated reads by workspace, source revision/hash and range/query.
  Record counts/byte sizes rather than secrets or raw prompts. Partially
  overlapping reads are diagnostic signals, not proven waste.
- Offline tests: nested child totals, cancelled/failed attempt, retry, repeated
  export, compaction, missing usage, cache/input overlap, reasoning/output overlap,
  unknown price, zero successes, verifier failure/disagreement/external cost,
  and deterministic report ordering.
- Bound collection/report memory and benchmark reporting overhead. Existing hot
  paths must remain provider-neutral. Add wire/store fields only if existing data
  cannot support a concrete requirement, with compatibility review.

## TE2–TE4 — low-risk waste reduction

TE2 begins with traces and schema snapshots. Test omitted fields, null/empty
values, range-mode conflicts, malformed cursors and provider-supported schema
subsets. Do not silently reinterpret a destructive command. RR10 owns coercion;
this slice makes legal invocation easier and errors smaller/actionable. Claims
from an external tool host must be reproduced against QQ before filing a bug.

TE3 briefs carry question, scope, known evidence, stop condition and response
budget. Results carry conclusion, evidence references, uncertainty and next
step. Enforce bounded output through existing budgets; a response word limit is
only guidance. Compare inline against independent bounded discovery. Preserve
configured/authenticated worker selection, explicit overrides, cancellation,
depth and authority attenuation. Do not add an automatic classifier initially.

TE4 selects one high-frequency format from TE1, not a universal log parser.
Include process status, signal/timeout, failing cases and locations, truncation
notice and exact-output handle. Test malformed/oversized/Unicode output,
nonzero exit with no parseable diagnostics, timeout, eviction and replay. A
summary cannot turn failure into success; retain bounded raw fallback and use
existing spill lifetime semantics. Measure parsing latency and failure recall.

## TE5–TE6 — evidence and context, opt-in experiments

TE5 starts with one source kind: immutable workspace-read evidence identified by
workspace, hash and range, plus producing run/tool identity. Do not broaden
session-scoped handles implicitly. Before code, review and record the explicit
parent/child read authorization, lifetime/eviction, revocation and restart
contract; a new authority or persistence boundary needs a separate accepted ADR.
No mutation result is automatically reused. Evidence text remains untrusted
content, never policy. Bound entry count, bytes and retrieval concurrency; avoid
blocking work on async workers. Tests cover same-content foreign workspace,
changed file, permission revocation, cancelled child, unavailable source, restart
and recovery without assuming the source still exists. A compact derived finding
must point back to exact evidence; do not store hidden reasoning.

TE6 compares against existing stale-read stubbing and compaction first. Limit the
candidate to a deterministic, versioned projection at a safe turn boundary;
retain the authoritative transcript. Preserve user constraints, steering,
open failures, pending approvals and tool-call/result pairing. Exact history
recall must remain possible under the existing retention contract. Persist any
new selection state required for identical reopen/replay. Test failure before
commit, cancellation, restart, summary rollback, source change, later request
for an omitted fact, repeated folds and unavailable exact evidence. Benchmark
context assembly and measure cache-prefix disruption; fewer logical tokens with
higher dollars/task is not automatically a win. Independent review is mandatory
for session/store changes. Do not ship a new summarizer merely to shorten logs.

## TE7–TE8 — recurring overhead

TE7 runs existing cache and tool-selection work together. Pin arm names with
namespaces (`delegation/`, `tools/`, `efficiency/`) to avoid A0 collisions.
Measure cold/warm requests, stable versus selected schemas, changed instructions,
and a compaction boundary. Report provider usage availability, cache-read/write
cost, TTFT and tool-discovery/recovery calls. A stable but large prefix may beat a
smaller changing one. Adapter cache details stay inside `qq-provider`.

TE8 selects one repeated mechanical sequence from TE1 (for example test output
to a verification receipt). Prefer an existing skill or `xtask` command over a
new exposed tool. A receipt identifies source revision, exact commands, exit
status and unverified work; it never marks checks that were not run as passed.
Cancellation, output bounds and approval apply to every underlying action. Do
not infer that changed-file test selection replaces required workspace gates.

## Experiment and promotion gates

1. TE1 defines a fixed, versioned corpus spanning localized repair, breadth
   discovery, multi-file work, noisy debugging and long-context constraint
   retention. Reuse D6b/T13 fixtures and retain their existing sample-size gates.
2. Offline fixtures and narrow benchmarks precede live experiments. Live runs
   require ENG-809 authorization of model routes, task subset, sample count,
   dollar cap and stop conditions. A small pilot screens candidates, not defaults.
3. Pin task commits, model/effort, tool/prompt config, provider, host conditions,
   retry policy and cache regime. Pair trials by task and seed; alternate arm
   order where practical. Report all failures, budget stops and missing data.
4. Before spending, record the primary metric, target improvement, success-rate
   non-inferiority margin and latency ceiling approved by the owner. No universal
   percentage is invented here. If these are unset, the run is exploratory and
   cannot promote a default. Bootstrap intervals are paired at task level, not
   independent turns; publish sample sizes and uncertainty.
5. A candidate must pass deterministic safety/retention tests and its owning
   plan's gates. Promote only when the predeclared efficiency target and quality/
   latency constraints are met. Inconclusive evidence keeps existing defaults.
6. Record before/after plus same-binary A/A for tail gates per the
   [performance runbook](../runbooks/perf-recording.md). Raw evidence goes under
   `target/qq-perf/TE<n>-<date>/`, not Git. Ledgers contain concise receipts.
7. Experiments are opt-in. A rollback restores the old projection/guidance/tool
   policy without deleting durable history or bypassing approval. Stop a live
   trial on safety/retention violation or spend cap, not only poor economics.

## Delivery order and completion

First: TE0 → TE1 → TE2/TE4. Reuse ENG-812's paid evaluation for TE3; TE7 joins
existing cache work rather than blocking the low-risk slices. TE5 and TE6 start
only after measurement identifies enough repeated evidence to justify them and
their boundary reviews pass. TE8 follows a demonstrated recurring workflow.

The program is complete when each hypothesis has an accepted measurement-backed
implementation or a recorded drop decision, not when all mechanisms exist.
Update Linear and ledgers per slice, amend as-built design only for shipped
behavior, and collapse this plan when closed. Proposed ADR-0042 does not
preapprove later authority, schema, model-routing or default-policy changes.
