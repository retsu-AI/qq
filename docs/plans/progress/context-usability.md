# Ledger — context usability

Plan: [`../context-usability.md`](../context-usability.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| C1 | 4 bytes/token estimate; summarizer planned against storage only; actionable rejection text | In review | [#56](https://github.com/retsu-AI/qq/pull/56) | Regression: 730 KB / 200k window sends; window-triggered auto-compaction and `/compact` past the window both run |
| C2 | Proactive compaction at 90 % of the window; in-run stale-read stubbing before a later turn fails | In review | `fix/context-usability-2-in-run-compaction`, stacked on #56 | Full mid-run summarization deferred: needs a mid-run cutoff marker in the store; stubbing recovers the common case (many reads) with no schema change |
| C3 | Audit default `off`; audit child bounded to 8 turns / 120 s | In review | `fix/context-usability-3-audit-default`, stacked on #58 | TUI already surfaces `RunAuditCompleted`; no client change |
| C4 | Anthropic `cache_control` and Bedrock `cachePoint` on system, last tool, last message block | In review | `perf/context-usability-4-cache-breakpoints`, stacked on #59 | `provider_encode` anthropic 195 → 204 µs (+4.6 %, three markers on a 1.1 MiB body); Bedrock gated to the Anthropic family |
| C5 | Leading read-only calls overlap in mixed turns; calls past 16 settle as not-executed tool errors; prompt names the cap and asks for batching | In review | `perf/context-usability-5-parallel-reads`, stacked on #61 | Prompt version 13 → 14; plan golden digest re-pinned (same as #50) |
| C6 | Occupancy survives pruning and checkpoints | Planned | stacked on C5 | |

## Entries

### 2026-09-16 — plan opened; C1

Motivation: a user-reported `729498 input tokens` rejection on a ~730 KB
transcript, plus five read-only investigations (QQ, Codex, pi, fx, OpenCode)
recorded in the plan's table. Branch from `b75ebac`.

C1: `ESTIMATED_BYTES_PER_TOKEN = 4` with `estimate_tokens` (ceil) and
`bytes_for_tokens` in `sessions/context.rs`; the in-run delta seed
(`lib.rs`) and the cross-run seed (`claim.rs::compatible_context_tokens`)
charge appended bytes at the same ratio. New
`CompactionDisposition::Summarizing`: the summarizer's request skips the
model-window check (both the reducible and the irreducible branch) and keeps
the storage backstop; used at the three compaction plan sites in
`execution.rs`. Rejection text rewritten: says "estimated", shows bytes, and
every reason names `/compact` or a new session; the false "already attempted"
wording is gone.
Tests: `context.rs` unit tests rescaled by the ratio (+3: ratio arithmetic
with the reported 729,498 case, summarizer-against-storage, recovery text);
`tests/compaction.rs` +2 regressions (window-triggered auto-compaction
completes; manual compaction past the window completes); two existing
window fixtures scaled ×4; two occupancy-delta assertions updated.
`cargo test -p qq-core`: 597 passed / 2 ignored.
Docs: `architecture.md` § run loop step 3.

Shipped: none. In progress: C1 (review). Blocked: none. Next: C2.

### 2026-09-16 — C2

Proactive: `context::plan` returns `Compact` for an `Eligible` prompt run
whose required tokens exceed `window - window/10`
(`PROACTIVE_COMPACTION_HEADROOM_DIVISOR`); every other disposition is judged
at the window itself, so exact-fit and already-compacted runs still send.
In-run: `prune_stale_tool_results` is now `pub(crate)`; the execute loop
records this run's read-only provider call ids and, on turn ≥ 2 when the
byte estimate plus output reserve exceeds the window, stubs results older
than `CONTEXT_PRUNE_KEEP_TURNS` in the live transcript, re-measures, and
drops the compatible-occupancy chain (a rewrite). Stored rows are untouched.
Tests: +1 planner unit test (headroom by disposition); +2 session
regressions (`a_run_that_outgrows_the_window_stubs_its_stale_reads_instead_of_failing`
— fails on the parent commit with the user's exact error class;
`a_prompt_inside_the_last_tenth_of_the_window_compacts_before_it_sends`);
`ReadNoteRepeatedly` script added to the harness. Workspace green.
Deferred: true mid-run summarization (`BetweenRunsOnly` stays for the case
stubbing cannot recover) — needs a store cutoff inside a run.
Docs: `architecture.md` § run loop step 3.

Shipped: none. In progress: C1, C2 (review). Blocked: none. Next: C3.

### 2026-09-16 — C3

`qq_config::AuditMode` and `AuditConfig::default()` flip to `Off`
(`qq_core::AuditPolicy` was already `Off`). The audit child's inherited budget
is additionally clamped in the run loop: `max_model_turns ≤
MAX_AUDIT_CHILD_TURNS = 8`, deadline ≤ now + `MAX_AUDIT_CHILD_DURATION_MS =
120 s`. Test: `audits_inherit_remaining_limits_and_charge_inclusive_spend_once`
asserts both bounds on the admitted auditor; config default test renamed.
Docs: `architecture.md` § audit, `protocol.md` § Final-answer audit,
`supervised-delegation.md` B1 gate text.

Shipped: C1 (#56 `d4fd971`). In progress: C2 (#58), C3. Blocked: none. Next: C4.

### 2026-09-16 — C4

Anthropic Messages: `system` becomes one text block with `cache_control`;
the last tool and the last block of the last message carry it; every other
block is unmarked so the transcript stays one growing cached prefix. Wire
shape change: single-text messages now serialize as block arrays (Anthropic
accepts both). Bedrock Converse: `SystemContentBlock::CachePoint`,
`Tool::CachePoint`, `ContentBlock::CachePoint` at the same three positions,
only when `supports_cache_points(model_id)` (any `anthropic` id segment,
including regional/global prefixes and inference-profile ARNs). Root
`ResolvedModel.prompt_cache.control` is `Native` for both APIs. Fixtures
under `qq-protocol/tests/fixtures` unchanged (no wire type change).
Tests: anthropic exact-body fixtures updated (+1 placement test); bedrock +1
system/cache-point test, +1 family gate test; root `resolved_model` asserts
`Native` for bedrock. Minimal provider profile green.
Gate: `provider_encode --quick` anthropic 195 → 204 µs, heap 1.39x → 1.44x
(three 30-byte markers on a 1.1 MiB body). Live `cache_read_input_tokens`
qualification is a paid run and is recorded when the stack is exercised.

Shipped: C1. In progress: C2 (#58), C3 (#59), C4. Blocked: none. Next: C5.

### 2026-09-16 — C5

Run loop: the tool phase splits `approved` at the first call that is not an
overlappable read (`take_while`), runs that prefix under `MAX_PARALLEL_READS`
(the existing `buffer_unordered` path), then the remainder in request order
(the existing sequential path). Read-only-only turns and mutation-first turns
behave exactly as before. Over-cap: `MAX_TOOL_CALLS_PER_TURN = 16` stays the
executable cap; `MAX_ADMITTED_TOOL_CALLS_PER_TURN = 64` is the protocol
bound; calls 17..64 get a `rejection` at `ToolCallStarted` (not counted
toward slice or budget tool calls) and settle through the existing rejection
path as `is_error` results. Prompt 13 → 14 adds the batching line and names
the cap; `plan.rs` golden digest re-pinned; headless test expects 14.
Tests: +`calls_past_the_per_turn_cap_settle_as_tool_errors_and_the_run_continues`
(20 reads → 16 results + 4 typed errors, run completes),
+`a_mixed_turn_overlaps_its_leading_reads_then_runs_the_rest_in_order`
(3 reads see `before`, edit, trailing read sees `after`, results in call
order); the old hard-fail test now exercises the 64 admitted bound.
Docs: `tools.md` § Loop Bounds and § Within a turn.

Shipped: C1. In progress: C2 (#58), C3 (#59), C4 (#61), C5. Blocked: none.
Next: C6.
