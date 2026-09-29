# ADR-0052 — Jev approval activation comes from the run's compiled plan

**Status:** Accepted
**Date:** 2026-09-28
**Deciders:** v0.1.5 release review (ENG-971, PR #214; Jev plan JV1)
**Supersedes:** [ADR-0041](0041-jev-delegated-approval.md) decision 5, the
sentence "Whether Jev is consulted is read from the held call's workspace
configuration per hold and cached per credential epoch". The rest of
decision 5 (composition at the root, one reviewer handle, a missing key
falling through at the first hold) and every other ADR-0041 decision stand.
**Implements:** [`tools.md` § Approval Policy](../design/tools.md#approval-policy);
[`plans/jev.md`](../plans/jev.md) JV1 (acceptance A1, activation part).
**Related:** [ADR-0047](0047-jev-approval-hold-lifecycle.md) (Proposed)
covers the wider consent and hold-lifecycle redesign. This ADR takes only
the activation fix, which ADR-0047 decision 2 also calls for, so it can ship
before that package.

## Context

ADR-0041 read activation from the held call's workspace configuration,
reloaded without the run's profile or overrides, and cached the answer per
workspace until the credential epoch changed. The 2026-09-25 audit
(design/jev.md § 3, finding 5) reproduced three failures. Turning
`jev_approval` off kept calling TypeSafe until a credential mutation or
restart. A profile's `jev_approval` was ignored in both directions. The
per-workspace cache had no bound. The epoch tracks credentials, not
configuration, so it could never have observed a configuration edit.

## Decision

1. Activation is the run's compiled plan: `jev_approval` after the same
   override, profile and top-level merging as `jev_review` and
   `jev_routing`, trust-gated. The composition root records it in the plan
   descriptor as `approval_delegate`, the Jev approval identity when on and
   absent when off (`DESCRIPTOR_VERSION` 11 → 12). `qq-core` carries it as
   an opaque identity string and sets `ReviewRequest::jev_approval` when it
   is present. Core still knows no Jev type, provider or endpoint; the value
   only means "the installed reviewer may consult its first delegate".
2. The reviewer never reads configuration. Its only cache is the TypeSafe
   client, one entry per credential epoch, and only a built client is
   cached: a missing key or a failed keyring read is retried at the next
   opted-in hold. The client is global, not per workspace, so the cache is
   bounded at one.
3. The value is in the plan digest, like `checkpoint` and `routing`: a run's
   durable `run_started` plan identity then says whether it authorized an
   external approval delegate (Jev plan acceptance A1), and an on-disk edit
   that changes only this value yields a new digest and a new plan.
4. A run keeps the plan it started with, and so do its routed reload and
   its owned children: they inherit it through
   `RuntimeLoadRequest::approval_delegate`, exactly as `checkpoint` and
   `routing` are inherited, so a configuration edit in between cannot turn
   Jev on or off inside one run tree. A loader that cannot honour an
   inherited identity refuses the load. A change applies to the next root run.

## Consequences

- An on→off edit stops Jev on the next run, with no restart and no
  credential change. A profile's off beats a top-level on; a profile-only
  on enables Jev.
- `qq-core`'s public embedding surface changes: `ReviewRequest` and
  `RuntimeLoadRequest` gain a field and `ApprovalDelegateSelection` is new,
  so an embedder's struct literals must add them. The commit is marked
  breaking (`!`). The crate is `publish = false` and every in-repo caller is
  updated.
- A run already in progress keeps sending holds to Jev after an edit turns
  it off. An immediate stop is ADR-0047's server-side Off (JV9), not this ADR.
- Follow-up: ADR-0047, if accepted, supersedes this ADR's scope along with
  the rest of ADR-0041 decision 5.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Keep reading config per hold but key the cache on configuration fingerprints | Still reloads without the run's profile, so profile activation stays wrong; adds a second configuration path beside the plan |
| Keep `jev_approval` outside the digest, in cache live bindings only | The first version of this ADR did that. Review showed the durable run record then cannot say whether a run authorized an external delegate (A1), and routed reloads and owned children re-read configuration. `checkpoint` and `routing` already set the precedent of being in the digest and inherited |
| Wait for ADR-0047's full lifecycle package | The off-switch defect is data egress after an explicit off; fixing activation alone is small and does not pre-empt ADR-0047's other decisions |

## Evidence / references

- Audit: [`design/jev.md` § 3](../design/jev.md#3-why-jev-mostly-hands-work-back), finding 5.
- Tests: `a_held_call_whose_plan_is_off_never_reaches_jev_whatever_was_cached`,
  `jev_approval_reaches_the_plan_from_config_profile_and_override_and_follows_edits`,
  `an_on_disk_jev_approval_edit_replaces_the_cached_plan`,
  `a_reload_keeps_the_parent_approval_delegate_whatever_the_file_now_says`,
  `a_transient_key_read_failure_is_retried_at_the_next_hold`.
