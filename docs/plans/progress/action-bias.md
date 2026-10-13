# Ledger — Action bias

Plan: [`../action-bias.md`](../action-bias.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| AB1 | Shell reads after a leading `cd` are reads for progress and audit | Planned | [ENG-1038](https://linear.app/retsu-ai/issue/ENG-1038) | | Start after AP5's window (closed 2026-10-13 17:42 UTC) |
| AB2 | Prompt v20: action bias, exploration budget, built-in mapping, one instruction search | Planned | [ENG-1039](https://linear.app/retsu-ai/issue/ENG-1039) | | |
| AB3 | `read_file` window fills the byte budget; line-limit marker | Planned | [ENG-1040](https://linear.app/retsu-ai/issue/ENG-1040) | | After AB2 |
| AB4 | Declared tools and prompt match the run's authority; ADR-0059 | Planned | [ENG-1041](https://linear.app/retsu-ai/issue/ENG-1041) | | After AB2; ADR-0059 reserved |
| AB5 | Root stall report asks for the next change | Planned | [ENG-1042](https://linear.app/retsu-ai/issue/ENG-1042) | | After AB1 |
| AB6 | Seven-day re-measure; paired run if ENG-809 authorizes | Planned | [ENG-1043](https://linear.app/retsu-ai/issue/ENG-1043) | | After AB1–AB4 + 7 days |

## Entries

### 2026-10-13 — plan opened

- Research snapshot `../../research/action-bias.md` from a read-only store
  open: schema 44, 412 prompt runs and 29,956 calls in the 30 days before
  2026-10-13; counts only. The reference trees were read at the revisions
  named in the research.
- Baselines (root unless stated):
  - reads before first edit: median 9, p90 57;
  - same-path re-reads: 1,631 of 3,669;
  - shell reads after an optional `cd`: 725 of 1,886;
  - instruction-file probes: 271;
  - root stall reports: 0 (2 slice checkpoints);
  - wall time: 23.7 min without spawning, 145.4 min with;
  - children account for 73 % of built-in reads.
- No code changed. ADR-0059 reserved in `root.md`. Linear program issue
  ENG-1037 is under milestone `Action Bias`. Plan PR: [#295](https://github.com/retsu-AI/qq/pull/295).
- Two independent read-only reviews of the plan against `ce768bb`. The
  first returned Request changes, with one blocking item: AB5's unknown
  notice fails assembly with `CODEC`, so it needs store schema 45. The
  second returned Approve once that item and S1–S7 were folded in:
  - AB1 closes write shapes misclassified as reads today;
  - AB2 respects pruned-read stubs;
  - AB3 measures the turn budget;
  - AB4 filters with a bool and declares no per-mode clause;
  - research totals reconciled to the prompt-run window.
