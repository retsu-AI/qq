# Ledger — Compaction

Plan: [`../compaction.md`](../compaction.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| CX0 | Plan, ADR-0056, ledger and baseline | Shipped (`d3de2996`) | [ENG-993](https://linear.app/retsu-ai/issue/ENG-993) | [#239](https://github.com/retsu-AI/qq/pull/239) | Same PR as CX1 |
| CX1 | Narrative plus rendered record; resolved output cap | Shipped (`d3de2996`) | [ENG-994](https://linear.app/retsu-ai/issue/ENG-994) | [#239](https://github.com/retsu-AI/qq/pull/239) | No schema or protocol change |
| CX2 | Cache-aligned summarizer requests | Shipped (`c37afe25`) | [ENG-995](https://linear.app/retsu-ai/issue/ENG-995) | [#250](https://github.com/retsu-AI/qq/pull/250) | |
| CX3 | Durable prune watermark | In review | [ENG-996](https://linear.app/retsu-ai/issue/ENG-996) | [#254](https://github.com/retsu-AI/qq/pull/254) | Schema 41 → 42; stacked on #252 |
| CX4 | `RunActivity::Compacting` | In review | [ENG-997](https://linear.app/retsu-ai/issue/ENG-997) | [#252](https://github.com/retsu-AI/qq/pull/252) | `PROTOCOL_VERSION` 30 → 31; takes AC14's compaction-activity item |
| CX5 | Live qualification | Planned | [ENG-998](https://linear.app/retsu-ai/issue/ENG-998) | | 7 days after CX3 |

## Entries

### 2026-10-02 — CX0 baseline (ENG-992)

The query below was run read-only (`mode=ro`) against the lead's store at
schema 40. It covers all 23 compaction runs, 2026-07-29 to 2026-09-30:
16 completed and 7 failed. The failures were trust (1), window overflow
before the fold (2), empty conversation (1), six-section validation (2),
and the summarizer cut at a 2 048-token cap four times (1). The resolved
model name is not stored on compaction runs, so routes come from the
session's provider.

| Route | Runs | Wall time | Turns | Output tokens | Turn-1 cache read | Summary bytes |
| --- | ---: | --- | --- | --- | --- | --- |
| Anthropic via LiteLLM (2026-09-23 … 09-28) | 5 | 110–144 s | 2 (turn 1 at the 8 192 cap) | 8.6k–11.5k | 0 (cache read on the continuation only) | 19.5–26.1 KB |
| Codex (2026-09-24 … 09-30) | 3 | 171–389 s | 1 | 5.5k–12.0k at ~31 tok/s | 0 | 22.3–35.5 KB |
| Earlier, before the 8 192 cap (2026-07-29 … 09-22) | 7 | 19–78 s | 0–2 | 1.2k–6.0k | mixed | 4.8–13.9 KB |

Completed runs only. `5fa344a8` (152.5 s, 3 turns, no usage recorded) is
left out because its route is unknown.

- Run `337b1fa3` (gpt-6.1-sol, effort max) took 389 s and wrote 12 003
  output tokens, 4 660 of them reasoning, for a 30 294-byte summary.
- Sections 4 (files) and 5 (errors) are typically 6–10 KB each, and
  section 6 retypes every user message.
- Cross-run cache: 6 of 18 Anthropic follow-ups within 5 minutes of the
  previous request mostly missed the cache. The cause is assembly-time
  pruning (ADR-0056 § Context).

```sql
SELECT substr(c.id,1,8) AS run,
       datetime(c.started_at_ms/1000,'unixepoch') AS started,
       round((c.finished_at_ms - c.started_at_ms)/1000.0,1) AS wall_s,
       (SELECT count(*) FROM model_turns t WHERE t.run_id = c.id) AS turns,
       json_extract(c.usage_json,'$.input_tokens') AS input,
       json_extract(c.usage_json,'$.cache_read_input_tokens') AS cache_read,
       json_extract(c.usage_json,'$.output_tokens') AS output,
       json_extract(c.usage_json,'$.reasoning_tokens') AS reasoning,
       (SELECT length(summary) FROM session_compactions s WHERE s.run_id = c.id) AS summary_bytes,
       json_extract(c.outcome_json,'$.type') AS outcome
FROM runs c
WHERE c.kind = 'compaction' AND c.finished_at_ms IS NOT NULL
ORDER BY c.started_at_ms;
```

The query is run with
`sqlite3 -readonly -header -column "file:$HOME/.local/share/qq/sessions.sqlite3?mode=ro"`.
CX5 re-runs it, and splits the cache-read share by turn once CX2 lands.

### 2026-10-02 — Coordination with the autonomous-core stack

PRs #232, #233, #235, #236 and #237 merged on 2026-10-02 before this
branch was cut, so CX0 and CX1 start from `2a672fe`. #232 took ADR-0054,
so this plan's ADR is 0055, reserved in `root.md`. #237 moved the schema
to 40, and CX1 adds no schema change. Open PRs that touch the same
contracts:
- #166: schema and `PROTOCOL_VERSION`;
- #170: `PROTOCOL_VERSION`;
- #231: reserves ADR-0053.

CX3 and CX4 coordinate their numbers through `root.md` when they open.

### 2026-10-02 — CX1 implementation and review (ENG-994)

Changes are as in ADR-0056 § Decision 1–4. An independent read-only review
found no blockers. Its should-fix items are resolved in this PR:
- **Bounded reads.** Paths are taken from the arguments in SQL
  (`json_extract`/`json_each`), so a large `write_file` body is never copied
  out of SQLite. Messages and the last reply are read only up to the bytes
  that fit.
- **Output cap.** The summarizer's output cap is held to an eighth of a
  declared window, never below 8 192
  (`context::summarizer_output_tokens`), so removing the fixed clamp cannot
  crowd out the transcript.
- **4 MiB check.** The check counts the largest record next to the
  narrative.
- **In-run shrinkage.** In-run steps count the record toward shrinkage.
- **Steering selection.** In-run steering is selected by replay's own rule:
  up to the first kept turn, or the cutoff when none is kept. This is
  tested with a turn-ordinal gap and with no kept turn.
- **Small budgets.** No record is rendered below 2 KiB. Every part stays
  inside its share, swept over windows from 0 to 1M tokens.
- **Search notes.** The record's notes no longer promise that
  `search_history` returns steering or whole messages.
- **Record header.** Only a record header at the start of a line is cut
  from a reply.

Gate `context_assembly` (release, `QQ_BENCH_ITERATIONS=300`,
10 000 archived runs, A/B/C × 3, the stored summary length varied through a
temporary bench-only change):

| Stored summary | assemble |
| --- | --- |
| 22 B (fixture default) | 71.0 / 71.1 / 57.2 µs |
| 30 KB (today's typical six-section summary) | 60.6 / 63.1 / 77.8 µs |
| 94 KB (30 KB plus a full 64 KiB record, the worst case) | 110.9 / 103.7 / 123.6 µs |

Typical new summaries are about 8 KB of narrative plus a record well under
its cap, so they land in the second row. The worst case adds about 45 µs
per assembly, which is negligible next to time to first token. Tests:
`cargo test --workspace` passed, 13 ignored; the compaction suite has 63
tests, 7 of them new.

### 2026-10-04 — Rebased on `main` after #231, #238, #240, #242 and #244

#231 merged with its decision-model ADR renumbered to 0055, so this plan's
ADR is now **0056**: file renamed, reservation moved in `root.md`, next free
0057. #240 and #244 moved the store schema to 41, so CX3's bump is now
41 → 42. CX1 still changes no schema or protocol.

### 2026-10-04 — Rebased on `main` after the #246 revert

#246 reverted #231, #227 and #243, so `main` no longer has the
decision-model ADR-0055. It is reopened as #249 under the same number, so
this plan keeps **ADR-0056**, and `root.md` holds 0055 reserved for #249.
None of the reverted content is in this branch.

### 2026-10-04 — CX2 implementation (ENG-995)

- **Request shape.** `RunCapabilities::summarizer(PromptPrefixKey)` replaces
  `without_tools` for compaction runs.
  - The request uses the prompt runs' system prompt and tools.
  - It fetches no context sources and sends no output contract.
  - The prefix key is derived from the session
    (`execution::session_prompt_prefix_key`).
- **Rejected calls.** Every summarizer call settles as a rejection and
  nothing runs. The text of that turn is left out of the summary, and a
  second turn with a call fails `ProviderProtocol`. `Runtime::summarize`
  (in-run) does the same.
- **In-run transcript.** The in-run summarizer gets the live system prompt,
  tools, and the full message prefix through the boundary. It falls back to
  the run alone only if that would not fit the window.
- **Soak fixture.** It now recognizes summarizers by their instruction
  instead of by an empty tool list.
- **Tests.** Six were added:
  - `a_between_run_summarizer_request_extends_the_prompt_request_it_follows`
  - `a_summarizer_that_calls_a_tool_is_answered_once_and_its_reply_text_is_dropped`
  - `a_summarizer_that_calls_tools_on_two_turns_fails_closed`
  - `an_in_run_summarizer_keeps_the_session_context_before_its_prompt`
    (fails if the full-prefix path is disabled)
  - `a_summarizer_run_sends_the_prompt_runs_system_prompt_and_tools_without_context_sources`
  - the in-run prefix assertion in
    `one_run_spanning_several_windows_compacts_its_own_turns_and_completes`

  Three existing tests changed: they asserted that summarizer requests had
  no tools.

Gates, on the same host against the CX1 tip `e0fd1a0a`:

| Gate | Base | CX2 |
| --- | --- | --- |
| `context_assembly` assemble @ 10 000 archived, 200 iterations, A/B/A/B | 63.6 / 58.6 µs | 58.3 / 62.3 µs |
| `turn_overhead` median ns/turn @ 10 / 100 / 1 000, default 3 iterations, B/A/B (host under load) | 74.9 / 73.0 / 68.3 ms | 69.3 / 72.4 / 68.5 and 71.6 / 68.1 / 67.9 ms |

Both are within noise. The live cache-read share is measured in CX5.

### 2026-10-04 — CX2 review (ENG-995)

An independent read-only review found no blockers. Its should-fix items are
resolved:
- **Abandoned reply.** A rejected-call turn now discards everything the
  summarizer wrote so far, in both paths. A cut reply followed by a call
  turn could otherwise join its fragment to the next summary. Regression:
  `a_rejected_call_turn_after_a_cut_reply_drops_the_abandoned_fragment`,
  which fails without the fix.
- **Replay data.** The in-run retry sends the rejected turn's replay data,
  which Anthropic thinking requires.
- **In-run fit check.** It now uses the loop's measured-chain estimate, and
  falls back to the run alone right after a provider window rejection.
- **Tests.** Prefix-key parity for a read child:
  `a_child_sessions_summarizer_uses_the_childs_own_prompt_prefix`, which
  fails if the child key is derived wrongly. `Runtime::summarize` gets unit
  tests for one call turn with replay, a second call turn, and a cut call
  turn.
- **ADR wording.** ADR § Consequences now states what is cached and when:
  pruning divergence until CX3, per-run system suffixes, and the fallback
  trigger.

Two wall-clock-timeout tests failed once under full-workspace load and pass
3/3 alone:
- `hard_cost_budget_cancels_an_unmetered_looping_child`
- `wall_clock_budget_settles_a_hanging_provider_without_a_final_response`

The full re-run gives 2 099 passed, 13 ignored.

### 2026-10-05 — CX0–CX2 merged; CX4 implementation (ENG-997)

#239 (CX0+CX1) and #250 (CX2) merged on 2026-10-05 as `d3de2996` and
`c37afe25`. ENG-993–995 are Done. The rest of the stack is cut from
`c37afe25`: CX4 first, then CX3 stacked on it. CX4 is independent of CX3,
so it goes first and can merge alone.

CX4 adds `RunActivity::Compacting` (`compacting` on the wire),
`PROTOCOL_VERSION` 30 → 31. There is no schema change: `runs.activity` is
free text written by `codec::run_activity_column`. A store written at 31
can carry `compacting` in that column, which an older binary fails to
decode while that run is active. This is the same exposure as every
earlier `RunActivity` addition, and protocol skew is already a hard
failure.
- **Who publishes it.**
  - Every compaction run publishes `Compacting` once, before its first
    provider poll. This covers between-run (auto and `/compact`) and
    in-run.
  - A compaction run never publishes its summarizer's provider activity;
    that was already filtered by `internal`.
  - A prompt run publishes `Compacting` just before it calls the in-run
    compactor. Its next turn publishes `WaitingForProvider` as usual.
- **Reducer.** A `run_finished` only clears the session's activity when
  it belongs to the run that finished. An in-run compaction run finishes
  while its prompt run still holds the session, and previously blanked
  the prompt run's label. Regression:
  `compacting_is_the_activity_until_the_run_moves_on_and_an_inner_compaction_keeps_it`,
  which fails without the guard.
- **TUI.** The composer rule and sidebar row read "compacting context".
  `the_composer_rule_carries_run_telemetry_notices_and_hints_in_priority_order`
  asserts it.
- **Headless.** JSONL carries the event unchanged. New golden:
  `completed_after_in_run_compaction`, with the prompt run's
  `compacting`, the compaction run inside it, and the prompt run's
  `waiting_for_provider`. Text mode prints nothing new: `[tool]` and
  `[jev]` lines report decisions, not liveness, and a compaction already
  ends in a durable `session_compacted` the JSONL consumer sees.
- **Core tests.**
  - `a_compaction_run_reports_compacting_and_nothing_else`: between-run
    events, plus a snapshot taken mid-compaction.
  - `one_run_spanning_several_windows_compacts_its_own_turns_and_completes`
    now asserts the in-run order.
- **Fixtures.** `v31` goldens were written; `v30` is retained
  decode-only. The Harbor traces were regenerated by
  `make_fixtures.py`; only `protocol_version` changed.
- **Coordination.** Open drafts #166 (schema 37, protocol 29) and #170
  (protocol 29) have been stale since 2026-09-25 and already conflict with
  main at 30/41. Whoever revives them takes the next numbers.

### 2026-10-05 — CX4 review (Codex, #252)

- **Sidebar.** A running session that had already streamed text kept
  showing that stale tail instead of "compacting context". `Compacting`
  now wins over the tail. The `sessions` golden pins it: "Write tests"
  streams, then compacts. The golden fails with the old ordering, which
  meets the plan's TUI-snapshot criterion that the rule-only assertion
  missed.
- **Failure kind.** An in-run compaction whose `compacting` write failed
  settled as `ProviderResponse`, though the summarizer was never polled.
  It now settles as `Server` ("failed to persist run activity").
  Regression test:
  `an_in_run_compaction_whose_activity_write_fails_settles_as_a_server_failure`.
- **Wakeups.** The in-run activity write woke settlement waiters, so a
  parent awaiting the child made a needless `run_outcome` query. It no
  longer notifies; the feed delivers activity, as on the normal path.

### 2026-10-05 — CX4 review, second pass (Codex, #252)

- **Prompt-run failure kind.** A store failure inside an in-run compaction
  still failed the prompt run as `Policy` and advised `/compact`. The
  compaction run was already `Server`. `InRunCompactionError` gains
  `Persistence`, which now covers start, activity, cancellation-read and
  commit failures, and the loop fails the prompt run as `Server` with the
  store error. The regression test asserts both outcomes.
- **Headless golden.** In `completed_after_in_run_compaction`, the
  compaction run's `RunStarted` and `RunFinished` summaries said
  `generating_response`; the runtime reports `compacting` there, because
  the session's active run is the prompt run. Both the fixture and
  `one_run_spanning_several_windows…` now pin `compacting`.

### 2026-10-05 — CX4 review, third pass (Codex, #252)

- **Started-but-unreported.** A compaction run cancelled between its start
  transaction and the cancellation re-read settled without ever reporting
  `compacting`. Every start path now writes `runs.activity = 'compacting'`
  and appends the `RunActivityChanged` in the same transaction as
  `RunStarted`: between-run auto, manual (`start_reserved_run` for a
  compaction run) and in-run. The separate appends in `execute_started_run`
  and `compact_in_run` are gone, and with them the activity-write failure
  path the first review fixed. Its regression test now injects a failed
  compaction start instead, and checks that the prompt run fails as
  `Server` without `/compact` advice.
  `a_compaction_run_reports_compacting_and_nothing_else` and
  `one_run_spanning_several_windows…` pin the adjacent sequences.
- **Reducer.** The inner compaction run's own `RunActivityChanged` replaced
  `SessionView.activity` with a run that was not the session's active run.
  The reducer now applies an activity only for the active run (or when no
  run is active). The client test reduces that event.
- **Golden.** `completed_after_in_run_compaction` now includes the
  `SessionCompacted` the runtime emits after the compaction run's
  `RunFinished`, under its run id. The runtime test pins that order.
- **Ledger.** CX0–CX2 rows now read `Shipped (sha)`, per the workflow's
  status vocabulary.

### 2026-10-05 — CX4 review, fourth pass (Codex, #252)

- **Harbor trace.** `compaction.trace.jsonl` still modelled mid-run
  compaction as unobservable: an unowned `session_compacted` straight
  after a model turn. `make_fixtures.py` now emits the v31 sequence:
  - the prompt's `compacting`;
  - the compaction run's `run_started` and its `compacting` (summaries
    report `compacting`), then its `run_finished` with usage;
  - `session_compacted` under the compaction run's id;
  - the prompt's `waiting_for_provider`.
  The converter treats the compaction step as a system step, leaves the
  trajectory's `run_id` as the prompt run, and still reports four steps.
  The wire-shape test `tests/harbor_atif_fixtures.rs` passes. The four
  harbor-dependent Python tests error locally as before, because the
  `harbor` module is not installed.
- **Design doc.** The CX4 slice lists `design/transcript.md`. A new
  § Run Activity says:
  - which run owns the activity, and that an inner compaction does not
    replace or end it;
  - the rail precedence, with `compacting context` over a stale tail;
  - that compaction draws no transcript row.
  `layout.md`'s rail table points to it.

### 2026-10-05 — CX3 implementation (ENG-996)

CX3 is stacked on CX4 (#252). Store schema 41 → 42 adds
`sessions.prune_through_ordinal` and `prune_through_turn`.
- **Assembly.** `prune_stale_tool_results` now runs on
  `context[..prune_limit]`, the assembled length through the watermark
  turn, so its four-turn window ends at that turn. The work is one row read
  per assembly. `append_run_turns` returns each turn's end index instead of
  taking an eighth argument.
- **Live overflow-prune seam.** The run loop yields
  `RuntimeEvent::ContextPruned { turn_ordinal }` after it stubs its live
  messages. The session layer commits the watermark at `turn_ordinal - 1`
  before the loop is polled again, so the stubbed request is never sent
  before its replay is durable. The watermark is monotonic.
- **Proactive-threshold seam.** On `ContextPlan::Compact`, a prompt run
  first moves the watermark to the newest turn before its prompt and
  reassembles, at most once per run. If that fits, it sends with no
  summarizer; if not, it compacts as before. This is new behaviour: a
  near-full window with re-derivable reads now costs no summarizer call.
- **Bug found and fixed.** A live run re-stubbed its own stubs on every
  later overflowing turn, so a stub named the earlier stub's size
  (178 bytes) instead of the real result's, while replay named the real
  size. `prunable_stub` now refuses content that is already a stub.
  `a_live_prune_moves_the_watermark_and_the_next_run_extends_it` caught it.
- **Upgrade.** A session upgraded to 42 starts with its watermark at its
  newest committed turn, so it assembles exactly as at 41. Sessions with no
  turns stay unset.
- **Tests.** The three new runtime tests fail when their seam is disabled.
  - `each_run_extends_the_previous_runs_last_request_until_a_seam`: seven
    runs, each first request extends the previous run's last one, and
    nothing is stubbed.
  - `a_live_prune_moves_the_watermark_and_the_next_run_extends_it`.
  - `the_proactive_threshold_stubs_stale_reads_before_it_compacts`.
  - `version_forty_one_gains_the_prune_watermark_at_the_newest_turn`:
    backfill, unset for empty sessions, and a bad shape refused.
  - The reference assembly oracle applies the same watermark
    independently.
- **Adjusted tests.** Tests of stubbing itself now set a seam with
  `mark_prune_seam`: `assembly_prunes_stale…`, `pruned_history_still…`,
  `measured_occupancy_survives…`, `repeated_call_ids_keep_pruning…` and
  `permanent_reserved_reload_failure…` (where the threshold seam would
  otherwise take the injected reload failure). The `context_assembly`
  bench seed sets the watermark at its newest turn, so it still measures
  full stubbing.
- **Gates.**
  - `cargo test --workspace`: 2 105 passed, 13 ignored.
  - Soak: 7 passed.
  - `fmt` and `clippy -D warnings` are clean.
  - `context_assembly`, paired against the CX4 tip on the same host:

| archived runs | base assemble | CX3 assemble |
| ---: | --- | --- |
| 10 | 60.5 / 52.4 µs | 55.4 / 56.2 µs |
| 1 000 | 56.8 / 60.8 µs | 57.3 / 92.4 µs (one outlier) |
| 10 000 | 67.7 / 60.3 µs | 66.0 / 62.2 µs |

  Within noise.

### 2026-10-05 — CX3 review (Codex, #254)

- **Stub detection.** The re-stub guard matched any result whose last line
  began with `[pruned: `. A large read-only result that happened to end
  that way was never pruned. `prunable_stub` now treats content as its own
  stub only when it is exactly the stub this call would produce: an
  optional header line, then
  `[pruned: <name> <arguments> returned <digits> bytes; <hint>]`. A real
  result in that form is stub-sized, so keeping it costs nothing.
  Regression test:
  `a_stub_is_never_stubbed_again_but_a_result_that_merely_ends_like_one_is`.
  Rebased onto the CX4 review fix.
