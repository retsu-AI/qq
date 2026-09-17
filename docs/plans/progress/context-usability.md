# Ledger — context usability

Plan: [`../context-usability.md`](../context-usability.md). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| C1 | 4 bytes/token estimate; summarizer planned against storage only; actionable rejection text | In review | [#56](https://github.com/retsu-AI/qq/pull/56) | Regression: 730 KB / 200k window sends; window-triggered auto-compaction and `/compact` past the window both run |
| C2 | Proactive compaction at 90 % of the window; in-run stale-read stubbing before a later turn fails | In review | `fix/context-usability-2-in-run-compaction`, stacked on #56 | Full mid-run summarization deferred: needs a mid-run cutoff marker in the store; stubbing recovers the common case (many reads) with no schema change |
| C3 | Audit default `off`; audit child bounded to 8 turns / 120 s | In review | `fix/context-usability-3-audit-default`, stacked on #58 | TUI already surfaces `RunAuditCompleted`; no client change |
| C4 | Anthropic/Bedrock cache breakpoints | Planned | stacked on C3 | |
| C5 | Concurrent read-only subset; soft 16-call cap; batching guidance | Planned | stacked on C4 | |
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
