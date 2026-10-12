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
| AF6 | Test audit | Planned | | one module per PR |
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
