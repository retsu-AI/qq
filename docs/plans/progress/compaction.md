# Ledger — Compaction

Plan: [`../compaction.md`](../compaction.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| CX0 | Plan, ADR-0055, ledger and baseline | In review | [ENG-993](https://linear.app/retsu-ai/issue/ENG-993) | `perf/eng-994-cx1-compaction-summary` | Same PR as CX1 |
| CX1 | Narrative plus rendered record; resolved output cap | In review | [ENG-994](https://linear.app/retsu-ai/issue/ENG-994) | `perf/eng-994-cx1-compaction-summary` | No schema or protocol change |
| CX2 | Cache-aligned summarizer requests | Planned | [ENG-995](https://linear.app/retsu-ai/issue/ENG-995) | | Stacked on CX1 |
| CX3 | Durable prune watermark | Planned | [ENG-996](https://linear.app/retsu-ai/issue/ENG-996) | | Schema 40 → 41 |
| CX4 | `RunActivity::Compacting` | Planned | [ENG-997](https://linear.app/retsu-ai/issue/ENG-997) | | `PROTOCOL_VERSION` 30 → 31; takes AC14's compaction-activity item |
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
  pruning (ADR-0055 § Context).

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

Changes are as in ADR-0055 § Decision 1–4. An independent read-only review
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
