# F02 — run duration enforcement

| Slice | Goal | Status | Branch / PR | Inputs |
| --- | --- | --- | --- | --- |
| F02 / ENG-782 | One execution deadline through preparation and all work, then safe drain | In review | [PR #57](https://github.com/retsu-AI/qq/pull/57), `fix/eng-782-f02-run-deadline` | main `b75ebac`; independent of F01 PR #55 |

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

## 2026-09-16 — implementation and review iteration

- One admission clock now spans loader, input/context/guidance, runtime work,
  compaction and re-preparation; expiry cancels dispatch and drains ownership.
  Guidance/skill blocking leases and attachment joining prevent early reuse.
- Red/green: held loader previously completed and sent a provider request after
  expiry; now settles Duration without sending. Shell baseline failed at 2.005 s;
  repaired 300 ms case passes. Approval, host read/mutation, context preparation,
  stalled/completed compaction, child-write drain and blocking preparation pass.
- Independent review caught a stream-polling gap during persistence stalls.
  New Linux regression failed (shell still alive at 2 s against a 1 s budget);
  an owned finite-only cancellation alarm makes it pass before store release.
  Store writes remain awaited; no polling or unlimited-run alarm was added.
- Core suite before the final alarm change: 598 passed / 2 ignored. Core Clippy
  all-targets/all-features passes after that change. Final full-workspace gates,
  review, remaining acceptance checks and PR are still pending.
- Supplemental unlimited-tool-loop A/B uses an untouched `b75ebac` worktree.
  It is a post-edit reconstructed baseline, not the pre-edit deadline receipt
  above, and cannot be claimed as compliance with pre-change perf recording.
  Host IO pressure is already above 20%; no quiet-host tail claim is planned.

#### F02 verification receipt — 2026-09-16

Base `b75ebac`; red-first commit `94b269d`; implementation commit `99675cb`.
Tests: 12 added, plus inherited-child write-drain case; final workspace 1,470 passed / 4 ignored.
Commands: `env -u NO_COLOR TERM=xterm-256color cargo test --workspace --quiet`;
`cargo fmt --all -- --check`; workspace all-targets/all-features Clippy `-D warnings`; workspace build.
Red/green covers shell, loader, stalled persistence, approval, hosts, context, blocking input/guidance/skill,
stalled/completed compaction, repair, child drain, terminal repoll and unconfirmed cleanup.
Independent source/spec/standards review approved; final post-hook-isolation suite passed.
Unlimited tool loop: 30 A/B pairs × 100 iterations, median batch means 69.046 → 60.869 µs;
A/A 57.398 / 61.345 µs; IO some avg10 16.73%. No speedup/tail or finite-alarm-cost claim.
Deviation: supplemental benchmark baseline reconstructed after edits; original red deadline captured before edits.
Initial full-suite cost-budget test hit its 2 s timeout; exact rerun 0.21 s and two full reruns passed; cause unproven.
Docs: architecture budget/cleanup boundary. No dependency, schema, descriptor or wire change.
Evidence: `target/qq-perf/f02-2026-09-16/`; full logs `/tmp/qq-f02-{workspace-final,clippy,build}.log`.
Published: PR #57; hosted CI pending at publication. Not merged.
Open: native platform execution and quiet-host performance stay separate qualification gates.
