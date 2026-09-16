# F02 — run duration enforcement

| Slice | Goal | Status | Branch / PR | Inputs |
| --- | --- | --- | --- | --- |
| F02 / ENG-782 | One execution deadline through preparation and all work, then safe drain | In progress | `fix/eng-782-f02-run-deadline` | main `b75ebac`; independent of F01 PR #55 |

## Acceptance and ownership

Own core runtime budget/capabilities/stream handling, session execution and
focused deadline tests, this ledger, and runtime documentation. Root authorizes
the necessary architecture documentation amendment in this slice; no schema,
provider branch, dependency, or protocol addition is planned.

The clock begins at execution admission before loading/preparation, not while
a root prompt waits in its session queue. It is never reset by preparation,
automatic compaction, model turns, tool calls, child waits, or output repair.
Expiry cancels execution; owned work must drain before typed duration settlement
and session reuse. Failed cleanup remains fail-closed.

Public-session acceptance: sleeping shell, pending approval, external host,
child-drain ordering, held loader and input/context preparation, compaction,
output repair, cleanup failure. Test-first; second-agent spec/standards review;
workspace tests/fmt/Clippy/build before PR. Baseline the original failure before
runtime edits. Measure deadline response and preserve the unlimited fast path;
do not interpret loaded-host timing as quiet-host tail qualification.

## 2026-09-16

- Created isolated worktree on current main; checked F01 CI (running).
- Read ENG-782 and verified scope; Linear CLI fallback uses the verified ENG
  team because the app connection requires reauthentication.
- Independent read-only investigation confirmed loader, attachment, approval,
  tool/host, child, and compaction paths require coverage. No production edits yet.
- Baseline: `cargo test -p qq-core duration_expires_during_shell_execution_and_drains_before_session_reuse -- --nocapture`
  ran one test and failed: a 300 ms limit left the shell active at 2.005 seconds.
  The test shuts down and drains the runtime before asserting the failure.
- F01 PR #55 hosted CI run 177 completed successfully; it remains a separate PR.
