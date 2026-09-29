# ADR-0047 — Explicit Jev consent and a durable held-approval lifecycle

**Status:** Proposed. Nothing in this document changes behavior; JV1–JV6
implement it. Pending maintainer acceptance.
**Date:** 2026-09-28 (drafted 2026-09-25 as #193's "ADR-0046", renumbered
because 0046 is MCP tool-set pinning).
**Would supersede:** [ADR-0041](0041-jev-delegated-approval.md) decision 5's
workspace/credential-epoch activation and decisions 6–7's final-only
approval receipt and accounting. Decision 3 is superseded only when a
separately qualified question/precision policy ships (JV3, JV8). All approval
ceilings and independent consent remain.
**Plan:** [Jev](../plans/jev.md) JV1–JV6, JV9. **Basis:**
[`design/jev.md` § 3](../design/jev.md#3-why-jev-mostly-hands-work-back).

## Context

Jev answers typed, probabilistic questions. It does not establish
authorization or prove software correctness. Approval holds, model routing and
completion review have different authority and failure contracts. The
2026-09-25 audit (design § 3) found a root task-context gap, stale approval
activation, human UI and headless intervention before Jev answers, and
precision and observability problems. Raising the acceptance rate by lowering
thresholds would not repair any of these boundaries.

## Decision

1. **Independent consent, default off.** Review, routing and approval stay
   independently opted in, even when a key is stored. A preset may combine
   only capabilities shown to the user as an explicit choice. It is not a
   model effort parameter. It cannot grant `jev_approval`, expand an approval
   mode, override managed restrictions, or make owned children more
   privileged.

2. **One effective configuration identity.** Activation is resolved through
   the existing trusted layering, profile and run path at the composition
   root. Core carries a small immutable capability and policy identity, never
   `qq-config` types or provider details. Cache validity covers actual
   configuration, trust and credential changes. Caches are bounded and use the
   existing invalidation; there is no workspace discovery on the per-tool hot
   path.

3. **Off is not inheritance.** A server-side session Off suppresses all three
   Jev roles for subsequent runs, above lower-layer configuration and profile
   defaults. Clearing it restores configuration and says so. Per-capability Off
   remains.
   - Active review and routing keep their compiled identity.
   - An explicit immediate stop uses cancellation and durable settlement
     before acknowledging quiescence.
   - Approval consent is rechecked before each new attempt or grant.
     Revocation withdraws pending delegation without approving the held
     action.
   - Previous exact grants stay visible and separately revocable. Remote work
     already sent may still be billed.
   - `/delegate off` stays distinct: it withdraws both approval delegates, not
     review or routing.
   - A remote client's environment cannot revoke server state.

4. **The server owns the hold lifecycle.** A hold and its intended phase are
   persisted before publication. Client attention follows *human-required*,
   not the mere existence of an awaiting tool. Logical states (the wire
   spelling is chosen with the implementation's protocol fixtures):

   | Current phase | Event or condition | Next phase / invariant |
   | --- | --- | --- |
   | No hold | Static forbidden or denied | Terminal denial; no delegate |
   | No hold | Authorized bypass or grant | Execute under existing policy; no inference |
   | No hold | Held call with an eligible delegate | Reviewer-pending with attempt identity; no human attention |
   | No hold | Held call without a delegate, or `ask_user` | Human-required immediately |
   | Reviewer-pending | Approved | Commit the exact resolution or grant and the attempt's spend before execution |
   | Reviewer-pending | Denied | Terminal denial where the mode permits; otherwise human-required under existing `ask` semantics |
   | Reviewer-pending | Abstain or unavailable | Next configured bounded reviewer attempt, otherwise human-required |
   | Any pending | Explicit human override | One committed outcome wins; the inference result loses or is cancelled; unknown spend is kept |
   | Any pending | Cancellation, deadline or restart | Existing truthful terminal outcome; no repeated billed attempt or tool execution |

   The TUI, JSONL and attached clients consume the same persisted, replayed
   phase. There is no client-only grace timer or guessed reviewer-presence
   flag. The human wait starts at genuine escalation and keeps the current
   no-server-deadline contract.

5. **Attempts are durable and budgeted.** Worst-case capacity is reserved and
   a pending marker recorded before remote dispatch. Known usage or unknown
   spend settles in the authoritative store, exactly once, whoever wins the
   approval race.
   - A failure before dispatch is not billed. An uncertain send after
     dispatch is not free. Recovery never retries an uncertain send.
   - Bounded: one Jev attempt, then one configured LLM fallback per unchanged
     hold; payloads, response bodies, deadlines and queues.
   - Repeating an equivalent tool call does not reset limits. Local attempts
     are not counted as main LLM turns.
   - An LLM fallback without a maximum price cannot run under a hard cost
     limit. A budget reservation is not a second actual charge.

6. **Evidence and authority are distinct.** Jev receives bounded
   authoritative task intent, the applied steering revision, delegated scope,
   the complete essential action details, current policy facts and relevant
   evidence references. Omissions and provenance are marked; there is no full
   transcript by default. Tool text or a model rationale cannot grant
   permission. Jev infers only semantic facts code does not establish. An
   optional changed-evidence recovery returns to the same agent loop under the
   same limits, never a second planner or a retry-until-approve loop.

7. **Remote scores are not local policy.** The remote choice, distribution and
   confidence are kept separately from the parser's disposition and the
   versioned local result.
   - The pinned service's documented precision is honored conservatively.
     Unknown or malformed input fails closed.
   - Concentration confidence is not an independent correctness probability.
     Correlated criteria are not multiplied. Malformed data is not normalized
     into an approval.
   - A threshold or rubric change requires held-out QQ outcome calibration
     and a new policy identity. Lifecycle defects are repaired first, without
     lowering the existing threshold.

## Consequences

This adds durable state and receipts and explicit configuration semantics.
The implementation therefore needs a wire and schema compatibility review and
historical fixtures. Versions are allocated at implementation against the
actual merge base; none is reserved here, and no migration ships with this
proposal. Core stays provider-neutral, and every surface uses the same runtime
and reducer.

It should eliminate UI and headless interventions that precede an automatic
decision, and make the remaining handoffs explainable. It cannot promise Jev
accuracy, authorize more work, guarantee a task completes, or remove justified
human consent. Billing, evidence egress and tail latency remain the costs of
opting in. Masking is not a complete privacy boundary.

Qualification follows the plan's
[procedure](../plans/jev.md#qualification-procedure-jv7-jv13). Success never
turns a default on.

## Alternatives considered

- **Lower thresholds to reduce prompts.** Rejected before the context and
  contract repairs and outcome calibration; it confuses model uncertainty with
  missing authority.
- **Prompt immediately while Jev races the human.** Rejected: it makes
  successful delegation interruptive, and clients cancel valid work.
- **A longer headless grace timeout.** Rejected: timing guesses duplicate
  server state and still fail during fallback, load, reconnect or an
  unavailable reviewer.
- **Credential presence, or one monotonic intensity knob, enables
  everything.** Rejected: consent, review frequency and authorization are not
  equivalent.
- **Require completion verification to approve tools.** Rejected: an
  assessment made after the result cannot authorize an earlier side effect.
- **A generic decision-engine crate or an independent recovery agent.**
  Rejected: the existing approval, routing, checkpoint and budget seams
  suffice.
