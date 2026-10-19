# v0.1.8 release preparation

Owner: release lead. Procedure: [release runbook](../runbooks/release.md).
Ledger: [progress/release-0.1.8.md](progress/release-0.1.8.md).

This plan does not bump the version, generate a changelog section, move guide
pins or tag. The bump PR (`cargo xtask release 0.1.8`) follows only when every
pre-bump gate below is closed. Delete this plan and its ledger, with their
navigation rows, once v0.1.8 publication is verified.

## Scope

Merged since `v0.1.7` (`dc41c332`), reconciled against main `7750ec7f`:

- Durable goals: G0/AC4 #308, G2.1 #309, G2/G3 #330, G4 TUI/headless #331,
  G5 live evidence #332.
- Opt-in structured tracing (`QQ_LOG`, AF5) #329.
- Auth/provider credential refactor (ENG-1066) #341–#353, behaviour-preserving.
- Fixes: install rollback #287, cooperative preparation cancel #293, keychain
  read coalescing #299, reviewed-run recovery after held-tool cancel #333.

## Contract deltas

| Contract | v0.1.7 | v0.1.8 candidate |
| --- | --- | --- |
| `PROTOCOL_VERSION` | 32 | 35 |
| `CAPABILITIES_VERSION` | 1 | 1 |
| `DESCRIPTOR_VERSION` | 15 | 15 |
| `STORE_SCHEMA_VERSION` | 50 | 58 |

## Pre-bump gates

| Gate | State | Needs |
| --- | --- | --- |
| G-flake: `goal_model_audit_late_and_stale_confirmation_cannot_achieve` intermittently returns `ShutdownTimedOut` (1/30 under `--stress-count 30`; failed main CI on `a9bff3ef` and `7750ec7f`) | Closed: [#366](https://github.com/retsu-AI/qq/pull/366) merged as `ed2b9d6c` (test fixture only: the held loader ignored cancellation; 40/40 stress iterations pass) | — |
| G-host: `headless::tests::closing_goal_host_drains_accepted_work_without_admitting_successors`, `compaction::maximum_goal_is_verbatim_immediately_after_three_in_run_summaries` and `soak::stopping_a_pending_summarizer_preserves_measured_turns_and_unknown_totals` time out deterministically on a loaded local host; hosted CI passes them | Open | Confirm on a quiet host, or file as AF6 timing-sensitivity follow-ups |
| G-333: reviewed-run recovery | Closed: merged as `7750ec7f` | — |
| G-size: minimal (43.87 MB vs 41 MB) and full (48.67 MB vs 48 MB) release binaries over budget; v0.1.7 was under both (see ledger) | Open | Maintainer decision: reduce size, or explicitly accept the gap as ENG-834 did for v0.1.7. Budgets are not widened |
| G-adr: ADR-0049 (durable goals) and ADR-0063 (tracing) are Proposed while shipped | Open | Maintainer decision: accept, or ship as Proposed and state so in the changelog |
| G-ci: main CI green on the bump candidate | Open | Hosted CI on the merged prep commit |

## Draft Upgrading block

Paste under the generated `## 0.1.8 — <date>` heading in the bump PR:

```markdown
### Upgrading
- Contracts changed since v0.1.7 (0ver: PATCH release, ADR-0051): `PROTOCOL_VERSION` 32 → 35, `STORE_SCHEMA_VERSION` 50 → 58; `DESCRIPTOR_VERSION` stays 15 and `CAPABILITIES_VERSION` stays 1.
- Upgrade clients and servers together and restart every long-running `qq serve`: a client refuses a server on a different protocol.
- The session store migrates forward on first open; v0.1.7 cannot open the migrated store. Before upgrading, run `qq config paths` and locate the actual `data` directory. Stop every QQ process (`qq serve`, the TUI, and any `qq run`) and copy `sessions.sqlite3` together with any adjacent `sessions.sqlite3-wal` and `sessions.sqlite3-shm` into a separate backup directory. If processes must remain running, use SQLite's consistent backup instead: replace both placeholders with absolute paths, verify the source file exists, then run `sqlite3 -readonly "/absolute/path/from/qq-config-paths/data/sessions.sqlite3" ".backup '/absolute/path/to/backup/sessions.sqlite3'"`. Never use a relative source path: it can select or create the wrong database. Rollback requires stopping all QQ processes, restoring the pre-upgrade backup at the original data-directory path without retaining migrated WAL/SHM files, and using the v0.1.7 binary. Keep the backup until the upgrade is verified.
- New: durable session goals. `/goal` in the TUI, `qq run --goal`, and `qq goal set|show|pause|resume|clear|approve-check` against a running `qq serve`; the `goals` configuration section sets defaults and managed `policy.goals` sets ceilings (see the configuration, CLI, headless and TUI guides). Goal completion checks need their own explicit `once` or `for-goal` authorization; ordinary tool approvals never authorize them.
- New: opt-in structured diagnostics with `QQ_LOG=info` (Unix only, off by default; see troubleshooting).
- Sessions with JEV checkpoint review that became unusable after a run was cancelled or the server restarted while a tool call awaited approval are usable again, unless a review had started or produced a verdict for that turn.
```
