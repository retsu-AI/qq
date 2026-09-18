# ADR-0030 — Explicit, independent Jev capabilities

**Status:** Accepted
**Date:** 2026-09-18
**Decider:** User direction for the stacked Jev review implementation
**Supersedes:** ADR-0028's credential-driven activation rule

## Context

QQ must remain a fast, fully capable harness without Jev. Registering a credential
is not consent to send subsequent workspaces' tasks and evidence for assessment.
The user requested implementing the review recommendations without overdesign.

## Decision

Use existing trusted configuration and profile layering. Review defaults off.
Credential setup only stores a credential. Explicit off overrides enabled
defaults without requiring credential deletion. `final` preserves normal tool
batching; `enforce` selects the strict tool-result and final-candidate policy.
Environment settings are captured in LoadRequest and included in plan cache
identity, not reread from the hot loop. Profile declarations participate in trust
fingerprints so changing activation requires renewed workspace trust.

Routing is a separate capability and cannot implicitly enable review. Its
implementation and acceptance are tracked by J6; this decision does not claim
that a routing transport foundation is a completed router. Passive assessment
uses the existing observer boundary, not an untracked task in the run loop.

## Consequences

- Ordinary runs neither resolve reviewer credentials nor construct its client.
- Users select the cost/quality tradeoff explicitly; a stored key is reusable.
- Strict unavailable outcomes remain visible; no silent downgrade to an
  unreviewed success. Jev never authorizes side effects or proves correctness.
- Disabled-path overhead and end-to-end enabled quality/cost need measurement.

## Alternatives

Credential presence as activation was rejected: it defeats explicit off and
applies across unrelated workspaces. A new settings service or universal hook
framework was rejected: existing layering, plans and observer contracts suffice.
