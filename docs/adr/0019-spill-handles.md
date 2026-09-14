# ADR-0019 — Spill handles are durable session state: cut tool outputs are stored with their result, cited by a content-addressed handle, masked inline and exact on explicit read

**Status:** Proposed
**Date:** 2026-09-14
**Deciders:** tool-layer plan T4
**Implements:** [`tool-layer.md` § D4](../plans/tool-layer.md#d4--spill-store-and-read_tool_result-t4),
[`tools.md` § Output Bounding](../design/tools.md#output-bounding)

## Context

T1 made every tool result pass one bounding boundary and marked each cut
with `…[qq: N bytes / M lines omitted; not stored]…`. The omitted bytes were
gone: a model that needed line 1 000 of a 90 KiB shell capture had to re-run
the command, paying the wall time and any side effects again, and could not
recover it at all if the command was not idempotent. Across the harness
survey, "re-run to see the middle" was the most common wasted turn after a
truncated result.

Three properties constrain the fix. The store is the authority
(ADR-0002/0003): a marker that names a handle the store does not hold is a
lie the model will act on, so the citation and the bytes must commit
together. Secrets are masked at the boundary (T1), and a stored copy that
were masked would make `.env` debugging through the tool impossible; but an
unmasked copy that reached context by accident would undo the masking.
Sessions are the isolation unit: a child session must not read a parent's
handle by copying a string out of its prompt.

## Decision

A complete tool output whose model-facing text was cut is a durable row in
the session store, written in the same transaction as the `tool_calls` row
it belongs to, and cited from the marker by
`t:<tool>:<call8>:<digest8>` — the first eight hex digits of the tool-call
id and of the SHA-256 of the stored bytes. `read_tool_result` reads it back
by handle, session-scoped, returning exact unmasked bytes; the inline
preview stays masked.

Concretely: `tools_spills(tool_call_id PRIMARY KEY, session_id, run_id,
tool, digest, content BLOB, content_bytes, omitted_from_line, created_at_ms,
evicted_at_ms)` at schema 28; `ToolOutput.spill: Option<SpillRecord>` carries
the unmasked text from the bounding boundary to `RuntimeEvent::ToolCallFinished`;
`finish_tool_call` inserts the row between the result `UPDATE` and the
event append. The runtime finalizes the marker (`finalize_spill_marker`)
before yielding, and only when a `SpillReader` is installed — direct runs
have no store, keep no spill, and their markers still say `not stored`.
Bounds: 8 MiB per item (`MAX_SPILL_ITEM_BYTES`), 64 MiB per session
(`MAX_SESSION_SPILL_BYTES`) with oldest-finished-run eviction that nulls
`content` and keeps the row, so a stale handle answers `spill_evicted`
rather than `spill_missing`.

## Consequences

- Positive: a truncated result is now a pointer, not a loss. Paging
  (`offset`/`limit`) and searching (`query`, `regex`) a stored output costs
  one read-only call and no re-execution. The digest in the handle means a
  handle from another store, or from before a re-run, cannot alias a
  different output. Deleting the session deletes its spills; `qq sessions
  prune` only touches sessions with no runs, which have none.
- Negative / risks: the store grows by the size of cut outputs, bounded at
  64 MiB per session. The result row and the spill are two copies of the
  head and tail bytes; accepted for the single-transaction guarantee. The
  turn-budget re-cut (`TurnOutputBudget`) happens after the row is
  persisted, so the in-context marker and the stored marker can differ in
  their omitted counts; both name the same handle. Shell captures are
  still cut at 128 KiB before they reach the boundary, so a spill of shell
  output is at most that.
- Follow-ups: T6/T7 may raise the shell capture cap now that the bytes
  have somewhere to go. `search_history` does not yet search spills. A UI
  affordance for the handle (open the full output) is a client concern.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Files under the session directory | A second authority next to SQLite; crash between the file write and the row leaves a marker without bytes or bytes without a marker. |
| Store the masked text | Explicit reads exist to recover exact content; a masked store cannot answer "what is in `.env`". The inline preview still masks. |
| Handle = tool-call id only | A re-run of the same call, or a handle copied from another store, would read a different output under the same name. The digest prefix pins the bytes. |
| Reject writes past 64 MiB | A tool result would fail because of history it did not produce. Evicting the oldest finished run's content keeps the current run whole. |
| Delete evicted rows | A marker in the transcript would then say `spill_missing` (sounds like a bug) instead of `spill_evicted` (the cap did its job). |

## Evidence / references

- `crates/qq-core/src/tools/output.rs`: `finalize_spill_marker`,
  `MARKER_RESERVE_BYTES`, `MAX_SPILL_ITEM_BYTES`.
- `crates/qq-core/src/tools/dispatch.rs`: `ToolOutput::bounded` (spill
  capture before masking), `ToolOutput::exact`, `SpillRecord::handle`.
- `crates/qq-core/src/lib.rs`: `cite_spill`, the `ReadToolResult` dispatch arm.
- `crates/qq-core/src/sessions.rs`: `finish_tool_call`, `store_tool_spill`,
  `read_tool_spill`, `MAX_SESSION_SPILL_BYTES`.
- `crates/qq-core/src/sessions/store/schema.rs`: `create_tool_spills_table`
  (schema 28).
- `crates/qq-core/src/runtime/spill.rs`: `SpillHandle`, `SpillReader`,
  `render_tool_result`.
- Tests: `spills_commit_with_their_result_and_read_back_exactly_within_the_session`,
  `spills_past_the_session_cap_evict_the_oldest_finished_content`,
  `a_cut_result_names_a_handle_the_model_can_page_and_search_exactly`,
  `a_finalized_marker_names_the_handle_and_resume_offset`.
- Gates: `tool_dispatch` 45.2 → 44.7 µs median; `store_output_batch` 88 → 87
  ms/batch (`target/qq-perf/t4-2026-09-14/`).
