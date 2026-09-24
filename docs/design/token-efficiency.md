# Token efficiency: baseline and opportunities

This research inventory describes QQ at `8089a0e` (2026-09-23). It is not
an implementation claim or a performance receipt. Future work belongs to the
[token-efficiency plan](../plans/token-efficiency.md); the decision proposed
for review is [ADR-0042](../adr/0042-verified-task-efficiency.md).

## Existing mechanisms

| Mechanism | Evidence | What this does not establish |
| --- | --- | --- |
| Hash-aware reads, outlines/ranges, bounded search, batched edits, spills and exact recall | [Tool contracts](tools.md); [tool-layer plan](../plans/tool-layer.md) | That defaults minimize tokens per successful task; T13 remains an evaluation owner |
| Provider prompt-cache breakpoints | [Providers](providers.md), prompt-cache discussion | Cross-turn cache-hit rate, prefix stability, or savings across compaction |
| Bounded delegation and experimental worker/depth arms | [Delegation plan](../plans/supervised-delegation.md); `benchmarks/arms/` | That more agents beat root-only work on tokens or dollars per pass |
| Durable history and bounded model-facing tool output | [Transcript](transcript.md); [ADR-0019](../adr/0019-spill-handles.md) | A shared cross-agent evidence protocol or arbitrary permission to read another session |
| In-run compaction at safe boundaries | [ADR-0039](../adr/0039-in-run-compaction.md); [run-reliability plan](../plans/run-reliability.md) | Real-model constraint retention over repeated summaries |
| Evaluation comparison and usage accounting | [Delegation evaluation](../plans/supervised-delegation.md); [evaluation program ENG-809](https://linear.app/retsu-ai/issue/ENG-809) | A complete, deduplicated task-tree waste report with uncertain usage explicitly represented |

These are source/document observations, not fresh benchmark results. Tracker
status and merged implementation can differ; the owning ledger and exact commit
must be checked before an implementation starts.

## Cost model

The unit of useful work is an independently verified root task, not a turn or
child completion. The cost envelope includes root, children, reviewers,
compactors, failed attempts, recovery, and the independent verifier that determines
success (including external verification). Advisory observers outside the run
need a separately labelled total if included; they must not rewrite run usage.

Repeated information has two costs: initial retrieval/output and later prompt
replay. Delegation can avoid parent reading while adding worker bootstrap,
coordination, redundant retrieval, and integration. Parallelism primarily
changes elapsed time; it is not intrinsically a token optimization.

Logical input, cache-read/write categories, output and reasoning are distinct
measurements. Provider categories can overlap: reasoning may already be part of
output, and cached input may already be part of input. Adding every reported
counter overcounts. Missing usage or prices are unknown, not zero. Estimated
prompt-component attribution is not provider billing.

## Opportunity hypotheses

1. **Avoid contract-repair turns.** Optional fields and mutually exclusive read
   modes should be easy to express correctly. Existing RR10 owns tolerant
   decoding; schema/prompt ergonomics need separate evidence before changes.
2. **Return actionable evidence, not bulk logs.** Bounded test/build summaries
   can retain failure location, exit status and a full-output reference. Parser
   failure must fall back to bounded raw output, not apparent success.
3. **Delegate selectively.** Independent, evidence-heavy questions with compact
   answers are plausible wins. Tiny lookups and tightly coupled implementation
   often duplicate context. Existing D6b owns the economic comparison.
4. **Reuse evidence with provenance.** Hash/range/source references can reduce
   repeated reads across a task tree. A reference needs explicit authority,
   freshness, retention and unavailable-result semantics; a content hash alone
   grants no access.
5. **Retire obsolete working evidence.** Superseded reads and resolved logs may
   no longer help the next turn. Existing stubbing/compaction are the baseline,
   not new features. Exact history, user obligations and tool pairing still
   matter; changing old prefixes can reduce cache hits.
6. **Stabilize recurring prompt cost.** Stable schemas and instructions can
   improve caching. Narrow tool exposure can shrink prompts but also introduce
   discovery calls. These must be compared together, not optimized separately.
7. **Replace recurring mechanical decisions.** Existing skills and automation
   can emit verification receipts or focused diagnostics. Trace evidence must
   justify an operation before adding another tool or generic workflow engine.

## Boundaries

Runtime policy and task lineage belong in `qq-core`; adapter usage normalization
and provider cache controls stay in `qq-provider`. The root composes settings.
Evaluation/reporting consumes those interfaces rather than adding provider
branches to the request loop. Exact public types, schema migrations and new
cross-session permissions require implementation-time review and, where needed,
a separate ADR. This inventory introduces none.

No savings percentage, adaptive router, new cache, context policy or delegation
default is established by this document. The first experiment can legitimately
conclude that the existing behavior is cheaper.

## Offline report coverage

`cargo xtask eval report` includes `efficiency_coverage` schema version 1.
Existing cost/token fields remain Harbor agent aggregates, labelled
`harbor_agent_only`; they are not complete verified-task billing. The report
explicitly records missing external-verifier usage, request-level lineage and
potential failed-stream usage. `verified_task_cost_complete` remains false.
Duplicate Harbor trial IDs across directories are rejected rather than charged
twice; distinct retry IDs remain distinct attempts. No runtime telemetry or
provider request shape changes are involved.
