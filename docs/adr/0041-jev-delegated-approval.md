# ADR-0041 — Jev as an approval delegate, for held calls only

**Status:** Accepted
**Date:** 2026-09-23
**Deciders:** delegated-approval DA5 (ENG-862)
**Supersedes:** [ADR-0030](0030-optional-jev-decisions.md) § Consequences, "Jev never authorizes side effects", for the `jev_approval` lane only
**Implements:** [`tools.md` § Approval Policy](../design/tools.md#approval-policy), [`architecture.md` § Extension Contract](../design/architecture.md#extension-contract)

## Context

ADR-0030 made every Jev capability an explicit, independent opt-in and
recorded that Jev "never authorizes side effects or proves correctness".
Review judges evidence after a tool ran; routing chooses a model before a
run. Neither touches the approval gate.

The approval gate, meanwhile, held ordinary side effects on a human even
when a `reviewer_model` was configured. The run-reliability audit (R07)
found four calls that died as `denied_timeout` at exactly 300 s because the
operator was not watching. DA1 made the reviewer's `Deny` final under `auto`
and `supervised`; DA3 let the operator choose who settles a hold
(`approval_delegate`); DA4 bounded what a delegate's `Approve` may record.
Those slices establish the delegate seam: one `ApprovalReviewer` that
answers `Approve`, `Deny`, or `Escalate` for a call the mode already holds,
inside the mode's ceiling.

The operator asked for Jev to be that delegate when it is configured, with
the model reviewer as the fallback when it is not. This ADR records that Jev
may authorize one bounded class of side effect, and exactly how.

## Decision

1. **A third, independent capability.** `jev_approval: true` is a sibling of
   `jev_review` and `jev_routing`, default off, trust-gated in project files
   and profiles, overridable with `QQ_JEV_APPROVAL=on|off`. It enables
   neither review nor routing, and they do not enable it. A stored TypeSafe
   key enables nothing by itself (ADR-0030).

2. **The lane is the held call, nothing wider.** Jev is consulted only for a
   call the session's approval mode already holds and the delegate setting
   routes to a reviewer (DA3). `Forbidden` shell shapes (ADR-0020), blocked
   hosts, managed `deny_*`, and `ask_user` are settled before any delegate
   and never reach Jev. `read-only` and `full` hold nothing a delegate may
   decide. Jev cannot widen a mode, lift a floor, or answer a question.

3. **Typed yes/no/abstain over the preview.** The request is one `choice`
   question over three labels (`approve`, `deny`, `abstain`) against the
   approval preview: the command or the diff, the host, the task brief, the
   names of recent actions, the session's grants, and the mode. Each section
   is bounded (8 KiB) and secret-masked; a request past 64 KiB abstains
   rather than truncating into a confident answer. The transcript is not
   sent. The answer is accepted only under the pinned contract (model
   `jev-1.13.0`, a normalized three-label distribution, the chosen label at
   the maximum), and only when both confidence and the winning probability
   reach 0.7. Everything else — `abstain`, low confidence, a malformed reply,
   a transport failure, a missing key, a 5 s timeout — falls through.

4. **Fail closed, never open.** Falling through means the composed
   `reviewer_model` reviewer is asked next, then the human. Jev is never
   failed open to approve, and its abstention reason travels with the
   eventual escalation so the human sees why. A Jev `Deny` is a delegate
   `Deny`: final under `auto` and `supervised`, advice under `ask` (DA3).

5. **Composition at the root.** `qq-core` learns neither Jev nor a provider.
   The root package composes `JevApprovalReviewer` in front of
   `ModelApprovalReviewer`; both implement the existing `ApprovalReviewer`
   trait and speak `ReviewDecision`. Whether Jev is consulted is read from
   the held call's workspace configuration per hold and cached per credential
   epoch, so one reviewer handle serves every workspace and a missing key
   falls through at the first hold instead of refusing startup.

6. **Who decided is durable.** `ReviewVerdict` carries a `DelegateIdentity`
   (`Reviewer` or `Jev`). A delegate `Approve` that records an exact grant
   (DA4) writes it with `source = 'jev'` or `source = 'delegate'`, so a later
   audit can tell them apart. The gate reads both as exact-match delegate
   rows; the per-run cap counts both. The resolution vocabulary on the wire
   is unchanged: `approved_by_reviewer` and `denied_by_reviewer` cover both
   delegates. DA6 decides whether the event gains a delegate field and
   whether that is a `PROTOCOL_VERSION` bump.

7. **Spend and clocks.** Jev's own clock is 5 s (the model reviewer's is
   10 s). Its spend is charged to the reviewed run through the same
   `GateDecision::Reviewed` path; when it falls through, its spend is added
   to the fallback's, with unknown on either side making the total unknown.
   The human's wait starts at the escalation, not when Jev was asked (DA1).

8. **The credential-free fixture refuses it.** `qq --tui-qa-root` rejects
   `jev_approval: true` and `QQ_JEV_APPROVAL=on` exactly as it rejects the
   other Jev capabilities.

## Consequences

- ADR-0030's "never authorizes side effects" now reads: Jev never authorizes
  a side effect except a call the approval mode already held and the
  operator explicitly delegated with `jev_approval: true`. Review and
  routing gain no such authority.
- An operator who opts in stops babysitting `auto` holds that Jev can
  confidently approve, and still decides everything Jev is unsure about.
- A new dependency on TypeSafe availability appears only on the held-call
  path and only when opted in; its failure is a fall-through, never a stall
  and never an approval.
- The pinned contract (model, labels, thresholds) is versioned in
  `JEV_APPROVAL_IDENTITY`; changing it is a new identity, not a silent
  policy drift.

## Alternatives

- Folding approval into `jev_review: enforce` was rejected: review judges
  evidence after execution and its `enforce` mode changes tool batching;
  approving before execution is a different authority and must be a
  separate consent.
- A new `ApprovalMode` variant was rejected: the mode is the ceiling and the
  five modes stay; who decides inside the ceiling is `approval_delegate`
  (DA3) and this capability.
- Letting Jev record prefix or workspace grants was rejected (DA4): a
  delegate's grant is exact-string, session-scoped, and never written to
  configuration.
