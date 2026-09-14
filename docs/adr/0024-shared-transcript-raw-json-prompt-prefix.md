# ADR-0024 — Shared transcript, raw tool JSON, and a precompiled prompt prefix on the request path

**Status:** Accepted
**Date:** 2026-09-13
**Deciders:** speed-first plan H18 (design D5)
**Implements:** [`speed-first-extensible-agent-harness.md` § D5](../plans/speed-first-extensible-agent-harness.md#d5--shared-transcript-and-precompiled-prompt-prefix-h18),
[`architecture.md` § Compiled Agent Plans](../design/architecture.md#compiled-agent-plans)

## Context

Every model turn built a `ModelRequest` by cloning the whole transcript
(`Vec<Message>`), every HTTP adapter cloned the request again inside its
restart loop, and the body serializer walked each tool schema and each
historical tool call's arguments as a `serde_json::Value` tree — a fourth
copy of that JSON — on every request. The system prompt (up to ~128 KiB) was
rebuilt from its parts and hashed in full per run. `provider_encode`, added
for this decision, measured the request path at 4.4–4.7x the transcript's
bytes in peak heap and 340–560 µs to encode one MiB; D5's gate is ≤2x heap
and ≤10 ms encode. The persisted `RunPromptIdentity.system_prompt_hash` and
the `PreparedStaticPrefix` that drives occupancy reuse both depend on the
prompt digest being a digest of the whole text, so any split must be exact.

## Decision

1. **The transcript is shared, not copied.** `ModelRequest.messages` is
   `Arc<Vec<Message>>`. The run loop holds the same `Arc`, gives each turn's
   request a clone of the handle, drops the provider stream before it
   appends, and appends through `Arc::make_mut`, which copies nothing once
   the stream is gone. A test pins that turn two's request carries the very
   allocation turn one saw. `Arc<[Message]>` was rejected because the run
   appends; a persistent-vector crate was rejected for one call site.

2. **Tool JSON is text, not a tree.** `ToolSpec.input_schema` and
   `ContentBlock::ToolCall.arguments` are `Box<serde_json::value::RawValue>`
   (the `raw_value` feature is enabled workspace-wide). Every HTTP codec
   embeds the text verbatim, or sends it as the JSON-encoded string the
   OpenAI Responses and Chat Completions shapes want, with no per-request
   serialization. The run already produced the canonical argument string when
   it validated the call; that string is what the transcript keeps, and the
   duplicate parsed `Value` on `PendingToolCall` is gone. The catalog measures,
   bounds, and digests schemas from the same text. The persisted
   `PersistedContentBlock` keeps `arguments` as a `Value` (serde's internally
   tagged enums buffer their content, which a `RawValue` cannot survive), so
   a stored turn parses once at load, not per request. Bedrock alone parses
   at request time because the Converse SDK wants a `Document` tree; it is
   behind the `provider-bedrock` feature and already deep-copied before.

3. **The prompt prefix is compiled, and its digest is continued.**
   `CompiledAgentPlan` holds a `PromptPrefix` per `PromptPrefixKey` (the
   `StaticFilter` of optional tools the run may see, or none, plus whether it
   may load guidance): the plan-constant text (header and tool names,
   progressive-exposure index, skill index, workspace instructions, persona)
   and the SHA-256 state of those bytes. The common session key is built at
   compile and counted in the plan's `estimated_bytes`; the other at most 31
   keys are built on first use behind a `Mutex` never held across an await. A
   run appends its suffix (selected guidance, context blocks, output contract)
   and finalizes a cloned hasher over the suffix, so `system_prompt_hash` is
   the digest of the whole prompt. Tests pin the digest and the text against
   the single-pass builder across four capability sets.

4. **Transcript strings escape themselves.** With `RawValue` fields present,
   `serde_json`'s per-byte string escape lost 30–60% on a one MiB body, and
   the loss appeared or vanished with unrelated build changes (identical
   instruction counts, different loop codegen). `providers::support::Text`
   wraps every bulk string the codecs emit: a word-parallel (SWAR) scan finds
   the next byte needing escape eight at a time, the clean run is copied
   once, and the literal reaches `serde_json` through its raw-value hook.
   Output is byte-identical; an exhaustive per-lane test holds it.

## Consequences

- Positive: `provider_encode` (one MiB, 32 schemas; 5–8 interleaved pairs vs
  `43caaea`, release profile): request heap 4.4–4.7x → 2.7–2.9x payload from
  an owned `Vec`, **1.55–1.80x from the shared `Arc` the run loop holds**;
  encode openai_responses 429 → 191 µs, openai_chat 466 → 212, anthropic 339
  → 194, google 559 → 406, stable across two candidate builds. Runs of one
  plan no longer rebuild or rehash the prompt body; `plan_compile` is +1.7%
  for building the common prefix once (22.1 → 22.5 µs) and
  `plan_estimated_bytes` grows by the prefix (16.4 → 20.7 KiB on the bench
  fixture). `provider_compiler` unchanged within noise.
- Negative / risks: `ContentBlock` and `ToolSpecInner` implement `PartialEq`
  by comparing schema/argument text, exact for `serde_json`'s canonical output
  and what every constructor produces. `Text` depends on the private
  `"$serde_json::private::RawValue"` token; a rename would make the literal
  serialize as an escaped string, which the unit test catches at once. The
  raw-value hook itself is what `RawValue` serializes through, so the
  dependency is the same one `RawValue` has.
- Follow-ups: `persist_model_turn` still re-measures the assistant message the
  runtime already measured (`sessions.rs`), a candidate for H21.2. The heap
  gate (≤2x) is met on the path the run loop uses; a caller that owns a
  `Vec` still pays the copy into the request and sees 2.7x.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| `Arc<[Message]>` | Cannot push; the run appends every turn |
| A persistent-vector crate | One site; `Arc::make_mut` is the same idea with no dependency |
| Keep `Value` and cache each schema's serialized form beside it | Two representations to keep equal; the text is the only one any consumer needs |
| `RawValue` in `PersistedContentBlock` | Fails to round-trip through the internally tagged enum |
| `#[inline(never)]` on the string serializer to pin `serde_json`'s loop | Recovered speed in one build and not another; the loop's codegen, not its inlining, was the variable |
| Hand-escape then re-validate through `RawValue::from_string` | Measured slower than baseline: the checked constructor re-parses the literal |

## Evidence / references

- `crates/qq-provider/src/model.rs` — `ModelRequest`, `ToolSpec::from_raw`,
  `ContentBlock::tool_call`, `PartialEq` impls, tests.
- `crates/qq-provider/src/providers/support.rs` — `Text`,
  `OwnedOrBorrowedText`, `write_json_string`, `clean_run_len`, tests.
- `crates/qq-provider/benches/provider_encode.rs` — counting allocator, four
  protocols, owned and shared heap.
- `crates/qq-core/src/runtime/prompt.rs` — `PromptPrefix`;
  `crates/qq-core/src/plan.rs` — `PromptPrefixKey`, `prompt_prefix`;
  `crates/qq-core/src/lib.rs` — `the_transcript_is_shared_with_each_request_and_grown_in_place`,
  `prefix_plus_suffix_digest_equals_the_full_prompt_digest`.
- Ledger receipt: `docs/plans/progress/speed-first.md` (H18, 2026-09-13).
