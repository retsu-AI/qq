# ADR-0056 — A compaction summary is a short model narrative plus an exact record that QQ renders from the store

**Status:** Proposed
**Date:** 2026-10-02
**Deciders:** lead; second reviewer required (session store, compaction commit)
**Implements:** [`compaction.md`](../plans/compaction.md) CX1–CX3. Amends how the summarizer request of [ADR-0039](0039-in-run-compaction.md) § 3 and the between-run step are built; the marker, ownership and fail-closed rules of ADR-0039 are unchanged

## Context

On the lead's store, a compaction takes 110–390 s (`progress/compaction.md`,
2026-10-02 baseline: 23 compaction runs, schema 40).
- **Output dominates.** The summarizer is told to retype every user message,
  every error and the file list, so summaries run 20–36 KB. On the
  Anthropic route via LiteLLM, turn 1 always hits the 8 192-token cap
  (`COMPACTION_OUTPUT_RESERVE_TOKENS`) at about 82 tok/s and is continued.
  On Codex, which omits `max_output_tokens`, one turn writes 8.6k–12k tokens
  at about 31 tok/s; one run took 389 s, 4 660 tokens of it reasoning.
- **Retyped data decays.** Every fold rewrites "verbatim or near-verbatim"
  user messages from the previous summary, so exactness depends on the model
  at every step.
- **The cache is cold.** The summarizer request has no system prompt and no
  tools (`RunCapabilities::without_tools`), so it shares no prefix with the
  prompt runs. Turn 1 reads 0 cached tokens on 152k–365k-token inputs.
- **Assembly pruning moves the prefix.** Between runs, assembly stubs every
  stale read-only result (`transcript.rs` `prune_stale_tool_results`), while
  a live run stubs only on overflow. A run's first request therefore
  rewrites the middle of the previous run's last request: 6 of 18 Anthropic
  follow-ups within 5 minutes mostly missed the cache.

## Decision

1. **The model writes a narrative only.** The instruction asks for five
   sections: Intent; Decisions and constraints; Work state; Open problems;
   Next step. It sets a target of about 1 200 words and forbids restating
   the record. The fold rule is stated: newer information wins, and anything
   not carried forward is lost. Validation requires these five headings and
   applies to the model's reply alone. A reply that echoes a record is cut
   at the record header.
2. **QQ renders the record.** At the compaction commit, in the same
   transaction, QQ renders a record from stored rows and stores it after the
   narrative in the same `session_compactions.summary` row. No schema change.
   - **Between runs:** every user prompt and applied steering message at or
     before the cutoff, verbatim and newest kept first. Older prompts that
     do not fit are listed by ordinal so `search_history` can be pointed at
     them, older steering is counted, and a message that does not fit is
     cut with a note. Then come the last assistant reply, the files
     modified and read (paths taken from `tool_calls` arguments in SQL), and
     the failed calls at or before the cutoff (first non-empty line,
     deduplicated, newest first).
   - **In-run:** the steering, files and failures of the replaced turns,
     selected by the same rule replay uses to drop them. The prompt and the
     kept turns are already verbatim.
   - **Bounds:** the record is at most 64 KiB
     (`context::COMPACTION_RECORD_BYTES`), and at most an eighth of a
     declared window. Below 2 KiB no record is rendered. Each part has its
     own share: the last reply 8 KiB, files 12 KiB, failures 4 KiB. User
     messages take what the other parts leave, at least about 39 KiB of a
     full record. Text is read from SQLite only up to the bytes that fit.
   - **Exact across folds:** the record is rebuilt from rows at every
     compaction and never folded, so it is exact after any number of
     compactions.
3. **Between runs, shrinkage is required of the narrative.** Above the
   16 KiB floor, the assembly measured with the narrative alone must be
   smaller than before. The record is a fixed, bounded cost: the storage
   envelope an eligible prompt reserves covers it, and the 4 MiB check
   counts the largest record next to the narrative. This guarantees that
   an old six-section summary always folds into the new format, because a
   record larger than the old summary cannot fail the step. An in-run step
   still counts the record toward shrinkage, because the live run keeps
   working in the same window and a running prompt never holds an
   old-format summary.
4. **The summarizer uses the run's resolved output cap**, held to an eighth
   of a declared window but never below 8 192
   (`context::summarizer_output_tokens`). The fixed 8 192 clamp is gone. A
   narrative fits in one turn, so the cap is a ceiling, and the window bound
   keeps the reserve from crowding out the transcript it summarizes.
5. **The summarizer request is cache-aligned (CX2).** It carries the
   system prompt and tool list of the session's prompt runs, built from the
   same `PromptPrefixKey`, and the session's reasoning effort. Context
   sources and an output contract are not applied to it. A between-run
   summarizer sends the assembled context plus the instruction; an in-run
   summarizer sends the run's live request cut before the kept turns, plus
   the instruction. Calls to declared tools are never run: each is answered
   with a rejection result (with the turn's replay data, which reasoning
   providers require), everything written up to that turn is discarded, and
   a second turn with a call fails the step closed.
6. **Pruning advances at seams (CX3).** Assembly stubs results only up to a
   durable watermark, which moves at the live overflow prune and the
   proactive threshold. Between seams, each request extends the last one.

## Consequences

- Old summaries keep replaying. The new preamble changes their replayed
  bytes, which costs one cache miss per session. The next compaction folds
  an old summary's narrative into the new format and rebuilds the record
  from rows, so facts the old summary retyped come back exact.
- The narrative can still be wrong. The preamble labels the record as exact
  and the narrative as condensed memory; `search_history` remains the
  recovery path.
- Shell-, `exec`- and MCP-made file changes are not listed. Only the
  built-in `read_file`, `edit_file` and `write_file` arguments are read. The
  narrative's Work state covers the rest.
- `search_history` walks prompts, replies and tool results but not steering
  text, and it returns excerpts. The record's notes therefore point at
  search only for prompts.
- Commit cost: one bounded read per part, at compaction time only. Assembly
  reads the stored row as before, which is now up to 64 KiB longer. On
  `context_assembly` at 10 000 archived runs, a 94 KB summary assembles in
  104–124 µs, against 57–78 µs for a 22 B–30 KB summary (ledger,
  2026-10-02).
- The in-run commit returns the stored text, so the live splice and replay
  render the same bytes.
- A summarizer request now pays for the tool declarations it does not use.
  On a cache hit they are cached tokens; on a miss they cost what the
  prompt run's first turn costs.
- The between-run prefix key is derived from the session (depth, purpose,
  approval mode, delegation depth), mirroring what a prompt run gets. A
  user prompt typed into a child session uses a different key, so its
  compaction misses the cache but is otherwise correct.
- What is cached. The tool block always matches the prompt runs'. The
  system prompt matches when the session uses no context sources, skill
  invocation or output contract, which are per-run suffixes a summarizer
  does not send. The messages extend the prompt run's last request unless
  a seam moved the prune watermark in between (§ 6). In-run, the messages
  are a prefix of the run's own last request.
- An in-run summarizer whose full prefix would not fit the window, judged
  on the loop's own estimate, or that runs right after the provider rejected
  that estimate, drops the session context before the prompt, as before
  CX2: a cache miss, not an oversized request.
- Pruning at seams (CX3, schema 42 → 43) adds one row read per assembly
  and one write per seam. A live run stubs its transcript only on
  overflow, and it records the watermark before it sends, so assembly
  replays the same stubs. A stub is never stubbed again, live or in
  replay, because the second stub would name the first's size. Between
  seams, read-only results older than four turns stay verbatim and the
  context grows by their bytes until the proactive threshold, which now
  tries stubbing before a summarizer. A session upgraded to 42 starts at
  its newest turn and assembles as before.

## Alternatives considered

| Alternative | Why not |
| --- | --- |
| A separate, cheaper compaction model | It cannot read the session's prompt cache. With a narrative-only reply, the session model is fast enough, and the summary keeps the session model's judgment |
| Lower reasoning effort for compaction | Effort is part of the provider's cache key on some routes, so it would split the cache. It also saves little once output is short |
| Keep the six sections and raise the cap | The cost is output length itself. A higher cap only removes the continuation turn |
| Store the record in its own column | A schema bump for data that is only ever replayed together with the narrative; a header line keeps one row and one read |
| Require shrinkage of narrative plus record | An old summary smaller than the new record would fail the step and never migrate |
| Keep the seeded file list in the instruction | The record lists files exactly; asking the model to annotate them is what made section 4 run 6–10 KB |

## Evidence / references

- Baseline query and table: `docs/plans/progress/compaction.md`.
- `crates/qq-core/src/sessions.rs` (instruction, sections, preambles);
  `sessions/compaction.rs` (validation, commit) and
  `sessions/compaction/record.rs` (record);
  `sessions/in_run_compaction.rs`; `sessions/execution.rs` (output cap).
- Tests: `sessions/tests/compaction.rs`. Five folds keep every user message
  verbatim without the model retyping one; an old-format summary folds when
  the record makes the assembly larger; record bounds and citations.
