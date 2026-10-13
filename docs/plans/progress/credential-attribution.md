# Credential attribution feature map

Diagnostics are explicitly enabled by `--credential-diagnostics PATH` and remain off by default. Typed process-local events describe existing read routes without serializing secrets, account identifiers, URLs, paths, prompts or arbitrary error text. The feature preserves credential results and read counts.

| Journey | Source | Verification |
|---|---|---|
| Stored owner, follower, cache and actual read | `crates/qq-auth/src/lib.rs` | Existing fake auth hook cases in `crates/qq-auth/src/tests.rs` |
| OAuth permit wait and backend return | `crates/qq-auth/src/codex.rs`, `xai.rs` | Existing held OAuth and waiter-abort cases |
| Call-scoped TypeSafe purpose with shared cache | `src/runtime.rs`, `src/runtime/approval.rs`, `routing.rs` | Checkpoint, routing, approval and concurrent ordinary-call regressions |
| Public activation and shared process identity | `src/cli.rs`, `src/main.rs`, `src/advisory.rs` | Existing CLI composition and separate-store fixtures |
| Advisory journal exclusion and owner release | `src/advisory.rs` | Existing journal replay and rejection targets cover second-writer refusal, retained descriptors, partial records, foreign identity and changed budgets |
| Exclusive private output and path-free errors | `src/diagnostic.rs`, `src/main.rs` | Existing activation boundary cases |
| Bounded records, loss summary and shutdown | `src/diagnostic.rs` | Existing queue, exact-budget, writer-failure and completion cases |

A held read emits no return event before it actually returns. Unavailable run, thread and cancellation context is omitted. The writer reports incomplete evidence on failure or bounded-close timeout. See the [diagnostic specification](../credential-attribution.md) for event and budget contracts.

Recorded checks and independent review apply to their exact revision. They do not prove authentication, native UI behavior or the original-session workflow.
