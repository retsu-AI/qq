# ADR-0045 — Select Strict verification explicitly and settle it atomically

**Status:** Proposed
**Date:** 2026-09-25
**Deciders:** Startup Manager, ENG-791
**Implements:** the original checkpoint contract alongside RR3 compatibility

## Context

RR3 deliberately permits completion with red or unavailable review evidence.
Existing `final`, `enforce`, and the Low–Ultrajev ladder must retain that policy.
The original ADR-0028 supported-verdict completion requirement needs a distinct,
explicit choice. Advisory audit and typed output validation cannot represent it.

## Decision

Add trusted `jev_review: strict` / `QQ_JEV_CHECKPOINTS=strict`, off by default.
Its pinned reviewer identity ends in `/strict` and is inherited by owned children.
Before provider work, require at least one explicit existing finite duration,
model-turn, tool-call, input/output/total-token, or cost limit. Do not introduce
a correction or review-count ceiling for Strict; legacy modes keep theirs.

Every retained tool outcome is assessed before an ordinary next turn. A concrete
failed tool may be supported evidence. Semantic rejection opens a durable
correction obligation; only fresh supported tool evidence closes it. A reworded
final candidate without that evidence cannot be assessed again. Repeating an
unchanged rejected observation cannot seek another reviewer score. Repairs retain
all original policies, approvals, workspace, cancellation and resource bounds.
Reviewer spend joins the existing meter and assessment waits respect the shorter
of five seconds and the original remaining duration.

Protocol32/store41 add a per-tool reviewed marker for bounded recovery and optional `VerificationRecord` alongside audit/output.
Checkpoint events commit the masked request basis, evidence generation, reviewer,
obligation and receipt. Only a final supported receipt with no open obligation
can become `verified`, in the same transaction as `Completed`. Otherwise use
verification-specific unresolved/unavailable failures. Cancellation, interruption
and budget exhaustion retain their true outcomes and non-verified records.
Recovery marks a pending assessment unavailable before terminal settlement;
uncertain inference is never replayed. A retained tool result that cannot reach
review receives an atomic local started/unavailable pair with no request basis or
remote spend claim. Review counts include these local attempts. Historical/non-strict
records remain absent.

## Consequences

- Strict completion has a durable qualifier across snapshots, events and headless
  outcomes. It is evidence support, not proof of truth or customer acceptance.
- Reviewer outage prevents Strict completion and does not trigger semantic repair.
- A child must receive a finite bound too: duration/token/cost remainder propagates;
  parent-only turn/tool limits do not silently become a fresh child allowance.
- Continuous operation, per-session Strict commands and `max_checkpoint_reviews`
  are deferred. No approval, routing, spending or execution authority is added.

## Alternatives considered

| Alternative | Reason rejected |
| --- | --- |
| Change `enforce` globally | Reverses shipped RR3 behavior and existing callers |
| Reuse advisory audit/output | Conflates separate guarantees and loses verdict state |
| Replace two repairs with 32 reviews | Reintroduces an arbitrary correction ceiling |

## Evidence / references

See `runtime/checkpoint.rs`, session streaming/settlement, and `strict_` regression
tests. Local test and review receipts belong in the ENG-791 root ledger; no live
TypeSafe/provider qualification or publication is established by this ADR.
