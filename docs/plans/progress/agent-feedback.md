# Ledger: agent feedback loop

Plan: [`../agent-feedback.md`](../agent-feedback.md). Linear: ENG-1028.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| AF0 | Baseline | Shipped (in stack) | `build/eng-1028-agent-feedback-loop` | `target/qq-perf/af0/` |
| AF1 | Compile and link speed | In review | same | |
| AF2 | nextest profiles | In review | same | |
| AF3 | `cargo xtask check` | In review | same | |
| AF4 | `cargo xtask smoke` | In review | same | |
| AF5 | `QQ_LOG` tracing | Planned | | ADR needed |
| AF6 | Test audit | Planned (remaining modules) | | one module per PR |
| AF6.1 | Cancellation module audit | In review | `test/eng-1028-af6-cancellation-audit` (stacked on #282) | deterministic wakeups, stress, mutation coverage |
| AF7 | CI affected/full split | In review | same | merge queue setting is the operator's |
| AF8 | AGENTS.md and skills | In review | same | |
| AF9 | `cargo xtask lint` | In review (first rules) | same | mistake mining next |
| AF10 | Skill eval | Planned | | |

#### 2026-10-11 session

- Baseline: `cargo test --workspace` 140 s warm. nextest 97 s with one
  failure: `qq-core sessions::tests::nonblocking::a_working_parent_receives_interim_reports_and_replays_them`
  hit its internal 30 s timeout under load and passed alone in 8 s. A docs
  edit relinked `qq` (4.5 s); the debug binary was 595 MB.
- AF1/AF2: dev profile with line tables only; `build.rs` exact tracking only
  for release. Results: docs edit rebuild 0.14 s, binary 189 MB, nextest
  44–81 s green on a host at load average 11–25.
- AF3/AF4: `qq-tui` change gate 46 s. Smoke: 6 scenarios in 1.6 s. An
  injected empty tool result failed `run-tool-call`, with the provider
  requests kept as evidence.
- One check run failed
  `qq-core sessions::tests::deadlines::duration_cancels_read_only_and_mutating_external_calls`
  (`deadlines.rs:597`, 2 s timeout waiting for the handler to enter). It
  passed 65/65 under stress, including under parallel `qq-core` load. Treated
  as a load flake and added as the first AF6 candidate.
- The `--tui-qa-root` drive and the tmux recipe in `verify-qq/features/tui.md`
  are unverified: tmux is not installed here.

#### 2026-10-12 review round

- Two independent reviews: a code review of the xtask gates, and a
  cold-start review of the skills and docs. Both asked for changes. All
  blocking and should-fix findings are addressed in `261af47c`.
- Real bugs found and fixed:
  - A missing merge-base passed untested commits.
  - The nextest parser saw nothing in the `ci` profile (`TRY n`, ANSI).
  - `#[cfg(test)] mod tests;` exempted the rest of a file from lint.
  - `--evidence` printed a stale report.
  - The serve-sse resume check passed on a closed stream.
  - The verify-qq fake's reply index never reset.
  - The serve.md cursor format returned 400.
- Lint now matches the old sleep count exactly, minus one false positive
  inside a string: 70. The ceiling must equal the count.
- Full gate after the round, at load ~11: 53.5 s with fmt, lint, clippy,
  2242/2242 tests, doctests, and minimal-provider clippy and tests (211/211).
  Smoke: 6/6 in 0.5 s.
- The TUI drive in `verify-qq/features/tui.md` is now verified through
  `script`.
- Still unverified: the tmux recipe (tmux is not installed here),
  `QQ_CONTEXT_WINDOW` auto-compaction, and the CI workflow itself (it
  runs on the PR).


#### 2026-10-14 PR #282 merge-readiness

- Merged current main without rewriting the shared PR branch; preserved the
  newer review rules in the AGENTS router. Addressed all 15 review findings:
  flaky CI fails, content-bound evidence, full-mode fallback, Windows paths,
  lint coverage, smoke assertions/deduplication/async logging, pidfd cleanup,
  and ordinary `.agents/skills` compatibility copies.
- Independent review found two missing docs_truth input mappings; both now
  select qq. Focused xtask tests: 95/95. Smoke: 6/6; repeated --only: 1/1.
  Full CI-profile gate: pass in 146.1s, including doctests and minimal provider.
- First unconstrained full run hit two delegation timeouts; both passed alone.
  Bound nextest to eight processes; full run passed with flaky-result=fail.
  Their expensive compaction fixtures remain AF6 candidates, not waived flakes.
- Corrected sleep baseline to 78: one compound-cfg sleep and three test-function
  sleeps previously missed, plus four existing sleeps from main; no new sleeps.
  Real verify-home fake/ask/stop passed. Windows and macOS not run locally.

- GitHub CI correctly rejected a retried-pass in the 50,000-directory
  depth-watermark fixture. Removed that duplicate stress-shaped test: the
  existing search::tests::ab2_depth_excluded_siblings_advance_the_scan_frontier
  covers depth exclusions, directory watermarks, cursor encode/decode, strict
  progress and final discovery over three pages with a deterministic 70-entry
  budget and a tiny tree. No production bounds or assertions were relaxed.
  Added parsing of nextest's actual FLKY-FL status observed in CI.


#### 2026-10-14 AF6.1 cancellation audit

- Owned paths: cancellation.rs tests, lint sleep ratchet, plan and this ledger.
  Stacked on #282 by explicit user request; parent must merge first.
- Replaced sleep and wall-clock assertions with explicit Future polling and
  observed wakeups. Covers 16 live waiters, future waits, repeated cancellation,
  dropping a registered waiter, and accurate cancellation diagnostics.
- Stress: 100 runs / 300 tests passed. cargo-mutants 27.1.0 generated six
  mutants: first pass 5 caught / 1 missed (Debug); diagnostic assertions added;
  final pass 6/6 caught, 0 missed, 0 timeouts. Evidence stays under
  target/qq-perf/af6-cancellation/mutants.out/; production code unchanged.
- Sleep ceiling 78 → 77. Full CI-profile gate passed; smoke 6/6.
  Remaining modules (especially shell-heavy compaction/delegation), AF5 tracing
  ADR, AF9 lesson mining and AF10 blinded eval remain open.

- Codex review: every cancellation waiter now owns a distinct wake counter;
  each counter must equal one before manually polling completion.

#### 2026-10-14 CI follow-through

- CI exposed three additional retried-pass fixtures. Search pagination now
  uses a thread-local, panic-safe test entry budget of 128 instead of creating
  50,000 entries; production bounds unchanged, original assertions retained.
- Blocking preparation deadline test freezes time until the hold hook enters,
  then advances past the deadline before asserting cleanup holds RunFinished.
  This removes the admission-vs-500ms race; attachment/guidance/skill all pass.
- Focused regressions pass; full CI-profile gate passes in 181.1s. Continuing
  until remote CI and Codex are green, not treating local gates as completion.


#### PR #282 rebase and latest review fixes

- Rebased with merge preservation onto current main, retaining action-bias wiring
  and restoring prior gate, smoke, and safe-cleanup fixes after conflict resolution.
- Peel annotated tag tips; require crate-level unsafe forbids; stream untracked
  evidence hashing; bound fake-provider retention and fail explicitly on overflow.
- Focused xtask suite passed 113 tests before added regressions; latest lint 11/11
  and smoke 4/4 passed. Formatting applied. Full CI-profile run was interrupted;
  no full-gate success claimed. User requested immediate push; remote CI pending.
