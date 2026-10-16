# Decision models: provider-neutral admission and PR319 review

Research snapshot: 2026-10-16. PR249 original head `d0f9ad8e`; PR319 head
`5ce07883`; fetched main used by the local rebase. This is source/documentation
research, not live-provider qualification. The planned contract lives in
[the decision-model plan](../plans/decision-models.md#provider-neutral-safety-contract-research-update-2026-10-16).

## Primary sources inspected

- [Cloudflare Clef model reference](https://developers.cloudflare.com/workers-ai/models/clef/):
  System One question types, 1–64 questions, account-scoped Workers AI endpoint,
  required model selector, 65,536-token context, embedded image limits and
  input-only pricing. Crucially, text state is truncated to fit remaining
  context after media/questions. API compatibility is not an evidence-integrity
  guarantee. These are vendor contract statements, not QQ measurements.
- [Cloudflare announcement](https://blog.cloudflare.com/clef-decision-models/):
  Clef and Clef-flash, claimed System One compatibility and vendor benchmarks.
  Do not use those benchmark numbers as QQ authorization qualification.
- [Clef model card](https://huggingface.co/Cloudflare/clef): open weights,
  local `/v1/systemone` serving, logits/softmax and a configurable encoder
  input bound (default 16,384 tokens). Hosted and local deployments need
  separate capability declarations and qualification, even for shared weights.
- [TypeSafe introduction](https://docs.typesafe.ai/introduction): state plus
  atomic typed questions; choice/score distributions and confidence, boolean
  probability; combination in code. Provider confidence is not a measured
  QQ false-approval rate.
- The attempted [OpenAI Decisions reference](https://platform.openai.com/docs/api-reference/decisions)
  redirected to the general API overview. No usable Decisions schema was
  verified in this research. Third-party search results are not a substitute
  for a primary contract; keep DM5 blocked rather than invent a payload or
  infer Codex credential applicability.

## PR249 source review

Verdict: refine the plan before implementation. Preserve ADR0055's accepted
provider/policy split and the explicit implementation gate. The latest original
head incorporates the earlier review's future-design placement, `/decisions`
acceptance, full calibration key, JV1 child dependency ordering and bounded
admission repairs. The rebase must preserve main's reorganized indexes and
current built-behavior fixes rather than restore old rows.

Additional design gaps addressed in the local plan amendment:

- A compatible wire does not prove absence of server truncation, correct
  deployment identity, credential audience or matching response envelope.
- A model's returned ID is necessary provenance, not attestation that a
  mutable deployment or fine-tune is still the qualified model.
- A decision must be invalidated by steering, revocation and changed action
  preconditions before execution; score reuse by argument template is unsafe
  as a foundation.
- A legacy threshold is not measured calibration. Preserve the extraction's
  baseline separately from promotion of new authority policies.
- “Never adds a serial wait” is impossible for inference that gates a side
  effect. Measure its critical-path cost and keep unrelated work parallel.

## PR319: useful parts and limits

Source-only review of `origin/main...origin/pr-319`; no contributor code,
script, installation, tests or provider requests executed. The contributor's
CI and independent-review statements were not reproduced. Treat contract
files as proposed evidence, not authority; byte agreement with another project
proves agreement, not correctness against the provider.

Useful ideas to port through the neutral design, not wholesale cherry-pick:

- Requested model and actual response model/request ID are distinct provenance.
- Explicit nullable usage remains unknown; malformed/missing required counters
  are not silently zero. Input-only pricing can remain known with unknown
  output usage when the verified pricing contract supports it.
- Persist model/request provenance in the authoritative approval transaction.
- Keep protocol fixtures and bounded response parsing. Test shared codecs via
  their real production entry points.

**Concrete should-fix:** `src/runtime/jev.rs:161–197` adds a conformance test
that reimplements a weaker validator rather than invoking production parsers.
It accepts missing usage keys as null, checks probability count rather than
exact labels, and omits type/confidence/winner consistency checks. For example,
renaming the `inspect` probability key to `delete` while retaining its value
passes that test validator. Production `nullable_count` at lines 62–70 rejects
missing keys. Route shared fixtures through production validation and add
missing-field, wrong-label, wrong-type, invalid-confidence and non-argmax rows.
This finding is source-derived, not runtime-reproduced.

Do not adopt its Jev-specific environment selector, fixed model allowlist or
alias acceptance as the neutral model-admission policy. `matches_response`
accepts either known version for `jev-latest`/`jev-preview`; provenance alone
does not qualify a newly returned version for authority. The plan must retain
shadow-only admission for unknown complete qualification keys. Likewise,
checkpoint provenance encoded into feedback prose should become bounded typed
fields, with escaping at presentation boundaries. Optional additive wire fields
still need the repository's actual protocol/fixture compatibility procedure;
“backwards-compatible” in a PR description is not that proof.

## Deliberate scope

No runtime implementation, new crate, wire/schema bump, paid evaluation,
threshold relaxation or automatic activation is authorized by these docs.
Clef admission and exact OpenAI support remain fixture-backed implementation
work. No claim of reduced interruptions or better model quality is made.
