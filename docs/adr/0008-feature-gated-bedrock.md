# ADR-0008 — Feature-gate the Bedrock family inside `qq-provider`; no provider-per-crate split

**Status:** Accepted
**Date:** 2026-09-02
**Deciders:** speed-first plan H1
**Implements:** [`providers.md`](../design/providers.md); [`architecture.md` § Provider Compilation](../design/architecture.md#provider-compilation)

## Context

`qq-provider` depended unconditionally on seven AWS SDK crates for Bedrock and
Mantle, so every embedder paid that dependency closure and binary size. The
alternative of one crate per provider would scatter shared HTTP, SSE,
redaction, and compilation code that the existing crate centralizes.

## Decision

One Cargo feature, `provider-bedrock` (default on), gates the `aws`, `bedrock`,
and `mantle` modules and the seven optional AWS crates. HTTP families need no
feature because they add no dependency beyond the shared `reqwest` transport.
A Bedrock recipe compiled without the feature fails at compile time with an
explicit message. The shipped `qq` binary enables everything; embedders build
`--no-default-features`. `test-support` provides loopback fixtures and is not
a public API.

## Consequences

- Positive: minimal artifact 38.7 MB versus 45.5 MB default; provider
  identity never branches in request hot paths.
- Negative / risks: two build profiles must stay behaviorally identical; the
  minimal profile test is part of the workspace gates.
- Follow-ups: none.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| One crate per provider | Splits cohesive protocol code; no measured build benefit |
| A `provider-http` feature | Would gate nothing |

## Evidence / references

- `crates/qq-provider/Cargo.toml:30-46`; `crates/qq-provider/src/lib.rs:11-12`;
  `crates/qq-provider/src/providers.rs:4-8`;
  `crates/qq-provider/src/compiler.rs:74-79`.
- Commit `5bb1471`.
- Test `bedrock_recipes_compile_only_with_the_provider_bedrock_feature`
  (`compiler.rs`); gate
  `cargo test -p qq-provider --no-default-features --features test-support`.
- Budget `qq_minimal_release_binary_bytes` ≤ 41,000,000.
