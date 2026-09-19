# ADR-0028 — Mandatory typed JEV checkpoints after tool results and final candidates

**Status:** Accepted
**Date:** 2026-09-18
**Deciders:** JEV runtime checkpoint slice
**Implements:** enforced TypeSafe JEV review profile

## Context

Prompt instructions and model-selected MCP calls cannot establish that every
tool result was reviewed. The required profile must stop before the next model
turn unless a pinned JEV assessment supports the exact result evidence, and it
must expose the correlated verdict to operators and replay consumers. This is a
domain decision boundary, not a general-purpose hook mechanism.

## Decision

Registering the endpoint-bound `typesafe-jev` credential with `qq jev setup`
installs one native `CheckpointReviewer` for subsequent runs; the explicit
`QQ_JEV_CHECKPOINTS=enforce` environment setting supports automation with the
`TYPESAFE_API_KEY` fallback. Enforced startup rejects a missing key. Each model
turn admits at most one executable tool call; every retained result, including
denied, malformed, unknown, MCP, read-only, and `spawn_agent` outcomes, is sent
to the pinned TypeSafe endpoint with a five-second deadline. Only `supported`
advances. The verdict is persisted as a correlated `checkpoint_reviewed` event,
and its bounded feedback enters the tool-result context before the next turn.
The root final candidate is reviewed with bounded masked retained tool evidence
before `Completed`.

Tool-result questions judge only whether that invocation outcome is concrete
and usable for the next step; they never demand proof of whole-task completion.
Final-candidate questions retain the strict whole-task evidence contract. A
semantic red triggers a corrective turn with the feedback in model context;
recovery is bounded by the run's explicit turn/time budgets and cancellation,
not a checkpoint-local attempt cap. A final retry requires a fresh reviewed tool observation; an
unavailable or malformed reviewer response fails immediately.

The exact task and complete per-tool request must fit the reviewer bound before
dispatch. QQ records `unavailable` and fails closed instead of silently
truncating either into an assessable request. Memoization keys the complete
typed request, including phase, correlation, tool identity, and error status.
If cancellation wins after a tool result is durable but before its checkpoint
is durable, the session records a local `unavailable` checkpoint stating that
no reviewer verdict was durably recorded, then settles cancellation. Direct `qq ask` keeps
answer bytes on stdout and renders checkpoint notices on stderr. The
`LoadedRuntime` embedding adapter propagates the installed reviewer into its
compiled profile; loading a session cannot silently discard enforcement.
Child execution uses that same compiled profile: its supported final checkpoint
is durable before the child settles, and the parent receives the
`spawn_agent` result only after that settlement.

The same durable settlement rule applies when a deadline, runtime failure,
provider failure, or premature stream end cuts off a pending review. QQ records
the local `unavailable` checkpoint before `run_finished`; it preserves the real
terminal outcome and never represents the local marker as a completed remote
assessment. A nominal completion with any such pending result fails closed.
Persistence failures while recording an ordinary checkpoint event remain
fail-closed but are not covered by this terminal-drain recovery path.

This supersedes ADR-0003's narrower statement that synchronous decisions are
limited to approval, validation, and budgets. Persist-before-publish remains in
force: checkpoint status is committed before clients observe it. JEV is an
internal runtime capability, never a tool call, so it cannot recurse.

## Consequences

- Positive: enforced runs cannot silently advance on missing, unavailable,
  partial, contradictory, or malformed JEV responses.
- Negative: enforced runs serialize tool work and add one bounded remote call
  per result plus one for the final candidate.
- Limitation: cancellation and failures before a tool result or final candidate
  retain their existing durable settlement and do not produce a JEV event.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| Prompt-only or model-selected MCP review | Voluntary and bypassable |
| Generic before/after hooks | Unbounded authority and unclear durability |
| Execute a multi-call batch then review | Later calls advance before the earlier verdict |

## Evidence / references

- `crates/qq-core/src/runtime/checkpoint.rs`
- `crates/qq-core/src/lib.rs` (post-result and final-candidate boundaries)
- `crates/qq-core/src/sessions/streaming.rs` (durable checkpoint event)
- `src/runtime.rs` (`TypeSafeCheckpointReviewer`)
