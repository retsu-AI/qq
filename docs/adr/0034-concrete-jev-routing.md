# ADR-0034 — Route only among declared, authorized choices

**Status:** Accepted
**Date:** 2026-09-18

Jev task routing uses one concrete TypeSafe adapter on the existing compiled
runtime. Review and routing have independent trusted default-off controls.
Disabled plans do not construct a router or resolve Jev credentials. Credentials
alone do not enable either capability.

The adapter considers the configured fallback and at most seven other authorized,
authenticated configured models, with at most 32 combined model/effort choices.
It performs no live catalog discovery. Explicit model/profile/worker choices and
explicit effort remain authoritative. Automatic effort requires both adapter
support and declared model `reasoning_efforts`; unknown support retains omission.
Descriptor 9 records the policy version and a candidate/constraint fingerprint,
so changed alternative-model metadata or pin intent invalidates cached behavior.

One masked task projection (16 KiB maximum) and bounded metadata form a request
of at most 64 KiB. Responses are capped at 64 KiB and five seconds. Invalid,
uncertain or unavailable choices retain the configured model. Initial confidence
and winning probability thresholds are 0.7; they are not accuracy claims.

Owned children inherit the parent's policy identity, including disabled. Later
user prompts resolve current configuration. Session routing persists and budgets
spend using ADR-0032. Direct `ask` remains ephemeral: it uses the same router,
ordinary selected-provider loader and core runtime, and reports routing spend on
stderr. It gains no durable-session or budget promise. No new execution engine,
SDK dependency, automatic review activation or tool authority is introduced.
