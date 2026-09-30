# ADR-0053 — Decision models get a provider seam in `qq-provider` and their own `qq-decision` crate

**Status:** Accepted 2026-09-30 (owner decision on PR #231: reverse the no-new-crate rule)
**Date:** 2026-09-30
**Deciders:** owner (accepted); a second reviewer still reviews DM1's crate boundary and any change to approval authority
**Implements:** [decision-model plan](../plans/decision-models.md) DM1–DM8;
design [`decision-models.md` § 5–6](../design/decision-models.md#5-architecture).
Reverses the Jev plan's "no new crates, no decision framework" non-goal and
[ADR-0047](0047-jev-approval-hold-lifecycle.md)'s rejected alternative "a
generic decision-engine crate". ADR-0047's other decisions stand.

## Context

QQ's Jev support is about 1,750 non-test lines in the binary: an approval
delegate, a task router, a checkpoint reviewer and an advisory observer.
Each builds its own untyped `json!` request and parses its own reply. Each
applies its own 0.7 threshold and sum-to-one check, and hardcodes the
endpoint and model.

- `src/runtime.rs:3610-3874`
- `src/runtime/approval.rs:30-405`
- `src/runtime/routing.rs:16-336`
- `src/advisory.rs`

The transport is a private `reqwest::Client` outside `qq-provider`, even
though that crate is the only retry and redaction owner (ADR-0005, ADR-0040).

On 2026-09-29 OpenAI announced a Decisions API (limited preview, schema not
yet published). OpenRouter, LLM Gateway and local System One servers
already expose Jev's wire, and more decision models will follow. Adding a
second vendor to the current layout means copying all four consumers' wire
logic again. Every open defect in `design/decision-models.md` § 3 is a
policy or evidence defect, and those need one owner.

## Decision

1. **Talking to a decision model is a provider concern.** `qq-provider`
   gains `decision.rs`, which holds:
   - a `DecisionProvider` trait, next to (not extending) the chat
     `Provider`;
   - neutral `DecisionRequest`/`DecisionResponse` types that keep raw
     vendor distributions and confidence unnormalized;
   - declared `DecisionCapabilities` for kinds, limits, distribution
     fidelity, rounding, pricing and alias stability;
   - a `DecisionError` that separates pre-dispatch failures from uncertain
     sends.

   The adapters are `providers/typesafe.rs` (the System One wire, which
   covers TypeSafe, OpenRouter decisions, LLM Gateway and local servers)
   and `providers/openai_decisions.rs`, written from OpenAI's published
   contract. Both use the existing `HttpExchange`, redaction and
   `AttemptPolicy`, and never resend after an uncertain send. `qq-auth`
   keeps credential resolution. A stored credential is never consent.
2. **Deciding is a `qq-decision` concern.** The new crate depends on
   `qq-provider`, `qq-protocol` and `qq-core`, and never on `qq-config` or
   `qq-auth`. It owns:
   - versioned typed rubrics per consumer;
   - one answer validator and QQ-computed confidence;
   - the calibration table keyed by (provider, model id, rubric id, effect
     class);
   - batching, budget admission, shadow scoring and the receipt builder;
   - consumer adapters implementing core's existing `ApprovalReviewer`,
     `TaskRouter` and `CheckpointReviewer`.

   Provider identity never branches inside it.
3. **Uncalibrated means shadow.** A (model id, rubric id) pair with no
   calibration row can only score, never settle. This covers a new vendor,
   a new version, and an alias whose reported model id differs from the
   calibrated one. Promotion is a reviewed data change with a new policy
   identity.
4. **Core stays neutral.** `qq-core` gains no trait. Its Jev-named fields,
   stage and `DelegateIdentity::Jev` are renamed to decision-neutral names
   in one mechanical slice, with protocol fixtures.
5. **Configuration is translated at the root.** Named `decision_models`
   and per-consumer settings are resolved by the composition root (or
   `qq-harness`, ADR-0050) into recipes and typed settings. `jev_review`,
   `jev_routing`, `jev_approval` and `QQ_JEV_*` remain aliases. Every
   consumer defaults off. `QQ_DECISIONS=off` wins everywhere.

## Consequences

- **Positive.**
  - Adding a vendor is one adapter, or just a config recipe, and needs no
    policy change.
  - JV3 precision, JV6 receipts and JV7 shadow are implemented once.
  - Embedders get decisions without the binary.
  - The OpenAI and Jev comparison becomes a shadow arm with exact
    receipts.
- **Negative / risks.**
  - A new crate, and a public trait in `qq-provider` that must stay small.
  - Moving the code must not change Jev behavior before JV slices intend
    it, so DM2–DM4 are refactors pinned by the existing tests plus
    recorded-reply fixtures.
  - The J8 off-path gate must be re-measured.
  - OpenAI's contract is unknown, so DM5 is blocked until it is published.
- **Follow-ups.**
  - `architecture.md` § Repository Layout and § Extension Contract, and the
    `AGENTS.md` repository map, gain `qq-decision` when DM1 lands. That is
    a root request.
  - ADR-0050's `ProductExtensions` becomes the carrier for decision
    consumers.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Keep adapters in the binary; add `src/runtime/openai_decisions.rs` | A fourth and fifth copy of the wire and policy code; no embedder access; transport outside the retry owner |
| Everything in `qq-provider` (transport and policy) | Rubrics, calibration and approval policy are not provider concerns, and would make `qq-provider` depend on `qq-core` types |
| Everything in a new crate (transport and policy) | Splits HTTP, redaction and retry ownership away from `qq-provider`, against ADR-0005/0040 and the user's direction |
| Treat decisions as chat calls with structured output | Loses distributions and the latency and cost profile; OpenAI and TypeSafe both position decisions as a separate API |
| A generic plugin registry for decision consumers | ADR-0004 rejects universal plugin traits; the consumer set is closed and each has a typed core seam |

## Evidence / references

- Duplication: `src/runtime.rs:3759-3772`, `approval.rs:327-341` and
  `routing.rs:136-149` (usage/spend); `runtime.rs:3790-3836`,
  `approval.rs:353-390` and `routing.rs:155-196` (distribution validation).
- `crates/qq-provider/src/http.rs:417`: `HttpExchange` is `pub(crate)`.
- Core seams: `crates/qq-core/src/runtime/checkpoint.rs:239` and
  `sessions/runtime.rs:28,527`.
- OpenAI [DevDay 2026 recap](https://openai.com/index/devday-2026-recap/).
- Precedent: [OpenClaw decision models](https://docs.openclaw.ai/concepts/decision-models).
