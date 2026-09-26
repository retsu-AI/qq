# ADR-0046 — Explicit Jev consent and a durable held-approval lifecycle

**Status:** Proposed — ENG-938 / JU0; no behavior changed by this document
**Date:** 2026-09-25
**Deciders:** pending maintainer acceptance
**Would supersede:** [ADR-0041](0041-jev-delegated-approval.md) decision 5's
workspace/credential-epoch activation, decisions 6–7's final-only approval
receipt/accounting, and decision 3 only when the separately qualified question/
precision policy ships. All approval ceilings and independent consent remain.
**Plan:** [Jev usefulness](../plans/jev-usefulness.md).

## Context

Jev answers typed, probabilistic questions; it does not establish authorization
or prove software correctness. Approval holds, model routing and completion review
have different authority and failure contracts. The
[pinned audit](../design/jev-delegation-audit-2026-09-25.md) found a root task-context
gap, stale approval activation, premature human UI/headless intervention and
precision/observability problems. A higher acceptance rate obtained by simply
lowering thresholds would not repair these boundaries.

Drafts [187](https://github.com/retsu-AI/qq/pull/187) and
[189](https://github.com/retsu-AI/qq/pull/189) add session/checkpoint behavior and
feedback, not this approval lifecycle. Their proposed ADRs 0044/0045 are not
adopted by this decision. The plan's [comparison](../plans/jev-pr-comparison-2026-09-25.md)
records reuse options and conflicts.

## Proposed decision

1. **Independent consent, default off.** Review, routing and approval remain
   independently opted in, even when a key is stored. Presets may combine only
   capabilities shown to the user as an explicit choice. They are not a model
   effort parameter. They cannot grant `jev_approval`, expand an approval mode,
   override managed restrictions, or make owned children more privileged.

2. **One effective configuration identity.** Resolve activation through the
   existing trusted layering/profile/run path at the composition root. Core
   carries a small immutable capability/policy identity, never `qq-config` types
   or provider details. Cache validity includes actual configuration/trust and
   credential changes. Bound caches and use existing invalidation; do not perform
   workspace discovery in the per-tool hot path.

3. **Off is not inheritance.** Server-side session Off suppresses all three Jev
   roles for subsequent runs, above lower-layer config/profile defaults. Clearing
   it restores configuration and must say so. Existing per-capability Off remains.
   Active review/routing retain their compiled identity. An explicit immediate
   stop uses cancellation and durable settlement before acknowledging quiescence;
   it never relabels an active Strict run as ordinary successful completion.
   Approval consent is rechecked before new attempts/grants; revocation withdraws
   pending delegation without approving the held action. Previous exact grants
   remain visible and separately revocable. Remote work already sent may still be
   billed. `/delegate off` remains distinct: it withdraws both approval delegates,
   not review/routing. A remote client's environment cannot revoke server state.

4. **The server owns the hold lifecycle.** Persist a hold and its intended phase
   before publication. Client attention follows human-required, not the mere
   existence of an awaiting tool. Proposed logical states (wire spelling is chosen
   with the implementation's protocol fixtures):

   | Current phase | Event/condition | Next phase / invariant |
   | --- | --- | --- |
   | No hold | Static forbidden/denied | Terminal denial; no delegate |
   | No hold | Authorized bypass/grant | Execute under existing policy; no inference |
   | No hold | Held call with eligible delegate | Reviewer-pending with attempt identity; no human attention |
   | No hold | Held call without delegate, or `ask_user` | Human-required immediately |
   | Reviewer-pending | Approved | Commit exact resolution/grant and attempt spend before execution |
   | Reviewer-pending | Denied | Terminal denial where mode permits; otherwise human-required under existing `ask` semantics |
   | Reviewer-pending | Abstain/unavailable | Next configured bounded reviewer attempt, otherwise human-required |
   | Any pending | Explicit human override | One committed outcome wins; lose/cancel the inference result, retain unknown spend |
   | Any pending | Cancellation/deadline/restart | Existing truthful terminal outcome; no repeated billed attempt or tool execution |

   TUI, JSONL and attached clients consume the same persisted/replayed phase.
   No client-only grace timer or guessed reviewer-presence flag. Human wait starts
   at genuine escalation and retains the current no-default-deadline contract.

5. **Attempts are durable and budgeted.** Reserve worst-case capacity and record
   pending before remote dispatch. Settle known usage or unknown spend in the
   authoritative store, with exactly-once accounting independent of who wins the
   approval race. Unavailable before dispatch is not falsely billed; uncertain
   billing after dispatch is not free. Recovery never retries an uncertain send.
   Bound attempts (one Jev, then one configured LLM fallback per unchanged hold),
   payloads, response bodies, deadlines and queues. Do not reset limits through
   repeated equivalent tool calls or include local attempts as main LLM turns.

6. **Evidence and authority are distinct.** Send bounded authoritative task intent,
   applied steering revision, delegated scope, complete essential action details,
   current policy facts and relevant evidence references. Mark omissions and
   provenance. Tool text or a model rationale cannot grant permission. Infer only
   semantic facts not established by code. An optional changed-evidence recovery
   returns to the same agent loop under the same limits, never a second planner
   or retry-until-approve loop.

7. **Remote scores are not local policy.** Retain the remote choice/distribution/
   confidence separately from parser disposition and the versioned local result.
   Honor the pinned service's documented precision conservatively; unknown or
   malformed input fails closed. Do not treat concentration confidence as an
   independent correctness probability, multiply correlated criteria, or normalize
   malformed data into approvals. Threshold or rubric changes require held-out
   QQ outcome calibration and a new policy identity. The plan first repairs
   lifecycle defects without lowering the existing threshold.

## Consequences and rollout

This adds durable state/receipts and explicit configuration semantics, so the
implementation needs wire/schema compatibility review and historical fixtures.
Allocate versions at implementation against the actual merge base; no version
is reserved here and no database migration ships with this proposal. Core
remains provider-neutral; all surfaces use the same runtime and reducer.

It should eliminate UI/headless interventions that precede an automatic decision
and make the remaining handoffs explainable. It cannot promise Jev accuracy,
authorize more work, guarantee a task completes, or eliminate justified human
consent. Billing, evidence egress and tail latency remain costs of opting in.
Masking is not a complete privacy boundary.

Qualify with explicit off/on arms, failure injection and independently verified
tasks under the [qualification procedure](../runbooks/jev-qualification.md).
Success does not flip any default on. Strict completion is an independent optional
product decision, not a dependency of these approval repairs.

## Alternatives considered

- **Lower thresholds to reduce prompts:** rejected before context/contract repairs
  and outcome calibration; it confuses model uncertainty with missing authority.
- **Prompt immediately while Jev races the human:** rejected; makes successful
  delegation interruptive and causes clients to cancel valid work.
- **A larger headless grace timeout:** rejected; timing guesses duplicate server
  state and still fail during fallback, load, reconnect or unavailable review.
- **Credential presence or one monotonic intensity knob enables everything:**
  rejected; consent, review frequency and authorization are not equivalent.
- **Require Strict verification to approve tools:** rejected; a post-result
  completion assessment cannot authorize an earlier side effect.
- **A generic decision-engine crate or independent recovery agent:** rejected;
  the existing approval, routing, checkpoint and budget seams suffice.
