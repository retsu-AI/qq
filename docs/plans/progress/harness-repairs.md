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

### F01 implementation and verification

- Reproduced successful result replaced by a later turn's error, then a
  shell result pruned using a later read's arguments. Both regressions pass
  with run/turn/call lookup and turn-local pruning metadata.
- Added legacy missing-result, restart, compaction input, and post-compaction
  history coverage; explicit stored effects override legacy-name fallback.
- Independent spec/standards review approved source/tests/docs; fixed its
  identified eager-copy regression before qualification.
- Workspace: 1,443 passed / 5 ignored; fmt, all-feature Clippy, build passed.
  Tests need loopback sockets and `env -u NO_COLOR TERM=xterm-256color`;
  initial restricted-environment failures are not code-fix claims.
- Performance comparison pending. Deviation: the baseline is built from
  pristine production sources at `20d4f6b` after implementation, with only
  the identical test fixture added; no pre-edit timing was captured.
