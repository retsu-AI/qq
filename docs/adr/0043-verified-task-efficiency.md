# ADR-0043 — Optimize verified task efficiency, not individual turn size

**Status:** Proposed
**Date:** 2026-09-23
**Deciders:** QQ lead / token-efficiency plan review
**Proposed by:** [TE0](../plans/token-efficiency.md); governs planned TE1 and
promotion gates, not an implementation receipt.

## Context

QQ already bounds tool output, supports prompt caching, compacts context and
delegates work. Smaller prompts or more concurrent agents do not necessarily
reduce the cost of a correct result: they can add recovery, duplicate discovery
or break cached prefixes. Existing D6b and T13 evaluations have owners, so a
second evaluation system would duplicate work. Provider usage categories also
overlap and failed streams can leave usage unknown. A shared measurement and
promotion rule prevents optimizing a cheap turn at the expense of the task.

## Decision

The proposed optimization unit is an independently verified root task including
all descendant, reviewer, compactor, retry, failed-attempt and success-verifier
work, including external verification. Reuse existing
evaluation/accounting interfaces and promote defaults only on predeclared paired
quality, cost and latency gates; retain current defaults on inconclusive evidence.

Reports distinguish logical tokens, cache categories, billed cost and estimated
prompt attribution. Missing data remains unknown; inclusive totals are not added
to their components. Total suite spend divided by verified successes includes
failed tasks. Zero successes has no finite cost-per-success result.

Efficiency work preserves authoritative history, approval, containment and
explicit model choices. Evidence reuse and context projection remain bounded,
opt-in experiments until separately qualified. This decision does not authorize
cross-session access, a store/wire change, a new cache, automatic model routing,
a paid run or any new runtime default.

## Consequences

- Positive: delegation, context, tools and caching share an end-to-end objective;
  safety and quality cannot be hidden behind lower token counts.
- Negative: accounting uncertainty and paired trials add work before promotion;
  no saving is promised by this ADR.
- Follow-ups: TE1 defines report fixtures; ENG-809 owns paid-run authorization;
  later authority/persistence changes need their own decision review.

## Alternatives considered

| Alternative | Why not now |
| --- | --- |
| Minimize tokens per turn | Ignores recovery, extra turns and unsuccessful tasks |
| Delegate every discovery task | Confuses parallel speed with total efficiency |
| Select the cheapest worker globally | Ignores task success and explicit route choices |
| Build a new telemetry/memory platform | Existing accounting, evaluation and spill mechanisms are the smaller starting point |

## Evidence / references

- [Baseline inventory](../design/token-efficiency.md), source `8089a0e`.
- [Delegation evaluation](../plans/supervised-delegation.md), ENG-812.
- [Tool ablations](../plans/tool-layer.md), ENG-813.
- [Evaluation program](https://linear.app/retsu-ai/issue/ENG-809).
- No new benchmark, live qualification or implemented saving is claimed.
