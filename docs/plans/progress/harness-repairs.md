# Harness audit repairs

Source: the 2026-09-16 harness scale audit (F01–F28). Each repair uses a
separate worktree and PR, revalidates current main, and preserves the lean core.

| Slice | Goal | Status | Branch / PR | Acceptance |
| --- | --- | --- | --- | --- |
| F01 / ENG-781 | Preserve turn-scoped tool results and pruning metadata | In progress | `fix/eng-781-f01-tool-result-replay` | Public session follow-up/reopen regressions; mixed-effect repeated IDs; workspace gates; independent review |

## F01 scope

Input: main `20d4f6b`. Own `sessions/transcript.rs`, focused session tests,
this ledger, and transcript design documentation. No schema, provider, or
wire changes. Keep assembly linear and preserve legacy/interrupted replay.
Performance: compare reconstruction with identical fixture inputs before and
after; no new tail budget. Other findings remain unstarted in this repair series.

## 2026-09-16

- Revalidated F01 on current main; fetched remote before creating isolated
  worktree `/tmp/qq-f01-tool-result-replay`.
- Linear app needs reauthentication; authenticated CLI verified project qq
  and team ENG, then created and read ENG-781. AGENTS.md's DEV key is stale.
- No runtime changes yet. Public test seam: SessionRuntime commands and the
  model requests delivered to a scripted Provider.
