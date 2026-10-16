# Runtime reliability and useful work

## Goal

Finish tasks, not merely keep runs alive: an unattended task survives outages,
restarts and repeated compaction, produces inspectable output, and stays
within explicit resource and authority bounds. This is the remaining-work
owner for former `autonomous-core`, `action-bias` and `compaction` plans.
Existing AC/AP/AB/CX IDs and issues remain valid; no issue is closed by this
consolidation. Durable goals retain their detailed [specification](goals.md).

## Baseline, not qualification

AP0–AP4.2, AC1–AC3, AC10/11/12.1/17, AB1–AB5/AB6.1 and CX0–CX4 are
merged. Their implementation contracts live in [architecture](../design/architecture.md),
[tools](../design/tools.md), [protocol](../design/protocol.md) and ADRs
0039, 0048–0050, 0054, 0056, 0059–0061. Proposed ADR status is not changed
here. Do not repeat implementation instructions or old version bumps.

Receipts and unresolved failures remain in the historical ledgers:
[autonomous core](progress/autonomous-core.md),
[action bias](progress/action-bias.md), [compaction](progress/compaction.md).
New work records evidence in [reliability](progress/reliability.md).

## Remaining delivery

| ID | Outcome and acceptance |
| --- | --- |
| AC0.2 | Finish resource/concurrency/fsync harness and H0 registration. AC0.2.1 recorder is shipped; measure rather than rebuild it |
| AC1–AC3 performance | Matched `context_assembly`, `tool_dispatch`, `turn_overhead` receipts as applicable, with same-binary A/A. Resolve AC1 dispatch +4.0% versus A/A +0.14%; merge is not performance acceptance |
| AC4 / G0 | Owned by [goals](goals.md): loop guard rejects next identical call after two identical errors or four identical executed triples; pauses after two non-novel 256-call slices. Alternating stable reads pause, distinct successful audit reads do not; requested reports are not novelty. T13 must show no completed-task regression before default promotion |
| AC5/AC6 | One continuation delivery: exactly one successor under races/idempotent commands; reject stale/completed/failed/goal-snapshot runs. Settle uncertain calls interrupted, never replay side effects; prompt once; carry cumulative budget remainders, original deadline and repair allowance. Auto-continue opt-in, durable/once-only after restart, cancelled by prompt/cancel, never for no_progress. Enforce configured max_continuations across the durable chain, including restart; tests stop at the count bound as well as the original deadline. Coordinate outcome/wire design with AC18 |
| AC12.2 → AC12.3 → AC13 | Complete config/model/reviewer extraction and drive_to_outcome. External host uses only qq-harness plus Tokio, inline RON to outcome in <100 lines; core-only example too. Typed load errors, public limits, gated bench exports, no public rusqlite types; startup/plan_compile gates |
| AC14 residual | Continuation, loop-guard, report and child-delivery surfaces: reducer tests, headless goldens, TUI snapshots per state. Headless output must also report compaction token usage and pause state; CX4 shipped only compaction activity, not these outcomes. G4 owns goal controls |
| AC15 | Measure commits/s and WAL bytes/s at 1/8/32/100 streams. Optimize only a demonstrated bottleneck; encoded once in transaction, byte-identical live/catch-up/restart replay including migrations. Any commit-grouping or WAL optimization must keep p95 first-token and delta latency within budget. synchronous changes need an ADR. Measurement-only closure is valid |
| AC16 / ENG-803 | Accept and implement ADR-0038 first: archive state, automatic idle archival, cursor-safe cascading deletion, approaching-limit signals and protocol/client surfaces. Then simulated-days multi-session retention soak with scheduled deletion: DB+WAL proportional to retained window. Archive alone insufficient; never trim live history or delete active/queued sessions |
| AC18 / ENG-870 | Incomplete calls get durable synthetic not-executed result and reissue request, never partial execution or duplicate effects. Decide honest non-completed exhaustion outcome first. Test cancellation/deadline, adjacent complete calls, repeat truncation, live/restart/replay equality |
| AC19 / ENG-874 | Bounded normal-terminal reasoning closure, unambiguous missing-ID repair and display-only reasoning cap; ambiguous/duplicate calls still refuse, authoritative provider/storage bounds preserved |
| AC20 | Identify an unanswered latency question and consumer, measure bounded overhead first. No persistent stats/wire/schema without benefit; disabled means no hot-path work. May close without code |
| AP5, AB6, CX5, Goal 7 | Qualification below, not more implementation scaffolding |

AC18/19 follow shipped AC3. Goals G0 → G2 → G3 → G4 → G5 use fresh
runs, not prompt continuation; #308/#309 are not established as merged by
this cleanup. G2's checkpoint is not whole-G2 acceptance.

## Qualification gates

### Scripted unattended/resource soak

One continuation chain: 2,000 turns, 5,000 calls, ≥40 in-run compactions,
three 10-minute outages and two process kills. End completed, zero
Failed/Policy, no duplicate tool execution. RSS <baseline +64 MiB; live
store growth linear in persisted bytes; harness overhead flat within noise
from turn 10 to 2,000. AC0.2 sets the measured storage constant and AC15 the
fsync budget. Do not invent numeric budgets after observing a candidate.
Core minimal features drop htmd's dependency closure and pass tests.

### AP5 — productive delegation

Use [run-output queries](../runbooks/run-output-queries.md) against the dated
2026-09-30 baseline. Preserve the recorded 2026-10-06 17:42:25 UTC through
2026-10-13 17:42:25 UTC window as pending evidence, not a new future deadline.
Qualify post-AP3b and post-AP4 normal use: no child silent stretch ≥320 calls;
no child with ≥64 calls silent until its final turn; every slice checkpoint
is a recorded report; delegating runs blocked on children <20% of wall time
using merged intervals (baseline 40%).

### AB6 — root action bias

Follow the [qualification runbook](../runbooks/action-bias-qualification.md).
Baseline: 30 days ending 2026-10-13 UTC. Candidate: seven full UTC days from
the next midnight after one pinned AB1–AB5 build is installed and prior
writers retired. Cutoff is window end +24h for both cohorts; do not query
early. Preserve source/executable/exporter/classifier identities and writer
provenance; recount both with the same extractor/classifier. Unknown
provenance, insufficient coverage or zero denominator is unqualified.

- Built-in reads before first observed built-in edit: median ≤6, p90 ≤30.
- Canonical-identity rereads ≤25%; lexical shell-read share ≤15% (command
  shape is not execution or mutation proof).
- At most one discovery start per permitted instruction-search root per
  editing run; failed/denied/interrupted starts count, cursor pages continue.
- Root stall notice after 64 qualifying non-progress calls.
- No increase in edit-failure share or mean non-spawning-root wall time;
  no decrease in completed-root share.

Unknown shell/external mutation paths, aliases and instruction discovery/
page/read coverage remain explicitly unqualified. AB6.1 exporter is shipped;
its H0 residual is ENG-834, not new exporter work. Paid prompt A/B also needs
an accepted comparator-treatment slice; never bypass normal identity checks.

The comparator-treatment slice must keep ordinary prompt-version mismatch
rejection. Both arms build from the same accepted AB1–AB5 runtime source;
a reviewed counterfactual changes only AB2 prompt treatment and its four
descriptions, retaining AB3 schemas/behavior and AB4/AB5 authority/notices,
including base prompt version 22. Label it historical prompt-19-versus-20
treatment on a fixed runtime, not two historical runtimes. Record exact
allowlisted identity pairs for source revision, executable hash, prompt/system
hashes, tool-schema hash and derived catalog identity; validate only these
pairs while preserving all other comparator checks and identical T13 fixtures,
model and configuration. Tests reject unrelated binaries, unlisted identities,
input-schema differences and unrelated runtime behavior. A pre/post-stack
comparison is AB1–AB5 treatment, not an AB2-only claim, and needs separate
identity pairs and authorization. Until accepted, paid comparison stays blocked.

### Context-usability live qualification

C1–C6 shipped on fixtures; the live gate remains open. In a real
Anthropic/Bedrock session, verify cache_read_input_tokens >0 on turn 2;
a roughly 700KB transcript on a 200k model must send and compact. Record
runtime/model identities, usage and compaction result in this ledger. CX5
route timing alone does not satisfy this cache/long-context gate.

### CX5 — compaction

After seven deployed days with sufficient normal use, compare per-route
p50/p95 wall time, output tokens, turn count and cache-read share with CX0.
Target p50 <30s on both Anthropic and Codex baseline routes; record causes of
misses. Historical earliest close 2026-10-14 17:34 UTC is not deployment
proof. Exact record/cache-prefix/watermark behavior is already shipped;
summary-quality experiments remain ENG-807 / token-efficiency TE6.

### Goal 7 — integrated live reliability

Seven consecutive normal-use days after repairs: harness-caused failures
<5% of prompt runs; zero wedged sessions or Exhausted admissions; tool
contract failures costing a model turn <0.5% of calls. Report provider sends
for completed and failed runs separately with sample counts; sends per
failed run must fall against the dated baseline. Pauses/interrupted chains
stay separate; retries do not become additional task completions.

The live multi-window retention task requires ≥3 compactions and reported
cost; coordinate with goals G5 and ENG-809 rather than duplicate paid runs.

## Constraints and completion

One runtime, persist before publish, no side-effect replay, no planner/DAG,
plugin lane or automatic swarms. Caller budgets remain explicit; this plan
does not introduce default root/child spend caps. Paid evaluations require
ENG-809 approval, credentials and spend cap. Fixtures do not prove live
productivity; release acceptance does not turn a failed performance gate green.

For each delivery: behavioral/failure tests, affected check with evidence,
real-binary smoke for visible behavior, and matched pre/post measurements
for hot paths. Include all forced type/wire consumers and reconnect snapshots
in the same PR, not only named starting files. Independent review applies to
session/store/approval/retry/cache changes. Retire this plan when remaining
acceptance is resolved, leaving contracts in design and receipts in Git.

Retirement additionally requires ADRs 0048, 0049, 0050 and 0054 to be
Accepted or explicitly superseded, with corresponding as-built design docs
updated. ADR-0038 acceptance/implementation remains part of AC16, not only
a deletion-soak prerequisite assumed complete.
