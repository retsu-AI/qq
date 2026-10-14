# Ledger — Action bias

Plan: [`../action-bias.md`](../action-bias.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| AB1 | Shell reads after a leading `cd` are reads for progress and audit | In progress | [ENG-1038](https://linear.app/retsu-ai/issue/ENG-1038) | | Start after AP5's window (closed 2026-10-13 17:42 UTC) |
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


### 2026-10-14 — #295 Codex review corrections

- Verified all eight unresolved findings against current implementation:
  unattended questions are denied; names search needs explicit ignored
  roots; default reads have a 4,000-line output bound distinct from the
  explicit 2,000-line limit; eval compare rejects prompt-version changes.
- Plan preserves analysis-only scope, rejects long branch mutation aliases,
  specifies open-ended default reads and one authority-neutral root notice.
  Direct runs omit `ask_user`. Paired paid evaluation now explicitly needs
  an accepted comparator slice and ENG-809 authorization before dispatch.
- Added and executed three missing historical-window qualification queries
  read-only (schema 44): root edits 211/1,230 failed; roots 190/254 completed;
  108 below-root editing runs, 44 instruction searches, 9 runs over one.
  Defined denominators, zero regression tolerance, window boundaries and
  unqualified cases. AB6 remains pending seven days after AB1–AB4 merge.
- Independent read-only review confirmed the eight corrections and caught
  one further write alias: `sort --output=out f` must count as work. Added
  separate and attached short/long output flag verdict cases to AB1.
- Documentation truth tests: 23 passed. Supplemental SQL also executed
  through the documented Bash/sqlite3 commands; outputs match the counts
  above. Relative links and `git diff --check` pass. Workspace gates running
  before push; no runtime code changed in this correction.


### 2026-10-14 — AB1 baseline started; schema dependency refreshed

- Previous continuation made progress: committed #295 corrections and
  independently reviewed their final state. Workspace gate process remains
  live; inspected its ongoing test output rather than restarting it.
- Isolated AB1 branch `fix/eng-1038-ab1-shell-reads` now inherits `147e4eca`.
  Baseline `cargo bench -p qq-core --bench turn_overhead` built successfully
  and is running; raw output is under that worktree's
  `target/qq-perf/ab1-2026-10-14/baseline.log`. Concurrent host work means
  this diagnostic cannot qualify performance acceptance.
- Fetched `origin/main`: `7de994b1` includes #289's schema 45 and #294's
  tolerant arguments. AB5 now specifies schema 45→46; historical schema-44
  research stays unchanged. Stack must integrate current main before its
  final qualification; no runtime code changed yet.


### 2026-10-14 — baseline test diagnosis and main integration

- Old-base workspace test gate failed: qq-core 933 passed, 5 failed,
  3 ignored; all five failures were 30-second fixture waits. Other gates
  after the test command were not run. Same compiled failing checkpoint
  test passed alone on disk in 14.13 s. All five unchanged binaries passed
  serially with test stores on task-owned tmpfs (under one second combined).
  Evidence: `target/qq-perf/ab1-2026-10-14/old-failures-tmpfs.log` in the
  plan worktree. I/O pressure was 43.63% some / 42.22% full avg10.
- Integrated `origin/main` (`7de994b1`) as merge `2ffb0a1a`, retaining both
  the updated autonomous-core index and action-bias priority row. No
  runtime fix was needed for this timing reproduction. Requalifying the
  integrated source with task-owned tmpfs test stores; disk latency remains
  unqualified by that correctness run.

- Integrated source workspace gates passed with isolated tmpfs test stores:
  2,272 passed, 0 failed, 14 ignored (35 test-report blocks); formatting,
  all-target/all-feature Clippy with `-D warnings`, and workspace build all
  exit 0. Logs: `target/qq-perf/ab1-2026-10-14/integrated-gate-{0,1,2,3}.log`.
  This qualifies correctness on tmpfs, not persistence latency on disk.


### 2026-10-14 — AB1 regression start

- Branch `fix/eng-1038-ab1-shell-reads` inherits integrated plan `a9dc6e80`.
  Pre-change tmpfs diagnostic on unchanged classifier: 212,429 / 163,549 /
  176,586 ns per turn at 10/100/1,000 (one iteration, unqualified).
  Original disk baseline recording remains live; do not confuse its handle
  with this separate storage diagnostic. Final matched performance gate is
  still open, including a baseline on the integrated immutable source.
- Added command-table and short-run sed audit regressions before the fix.
  No approval permission changes are intended; reads affect only progress
  reports and heuristic audit selection. Linear ENG-1038 is In Progress.

- Test-first result on integrated source: `cargo test -p qq-core ab1_
  --no-default-features` (tmpfs stores) gives 1 passed / 2 failed: command
  table and `ab1_cd_prefixed_reads_report_on_request_sixty_five` reproduce
  the intended failures. Log: `target/qq-perf/ab1-2026-10-14/ab1-red.log`.
  Short-run in-place sed audit preservation already passes.
- Original disk baseline completed, exit 0, three iterations: medians
  82,124,330 / 81,953,505 / 101,346,859 ns per turn at 10/100/1,000.
  Heavy storage contention and the pre-integration build make this
  characterization only; matched integrated-source qualification stays open.


### 2026-10-14 — AB1 classifier implementation

- Implemented a bounded, allocation-free lexical read classifier: one
  leading literal cd, quote-aware segments, read argv checks, shared exact
  Git branch listing predicate. Rejects dynamic/uncertain syntax and known
  write/execute flags; approval decisions are unchanged.
- Five focused tests pass, including request 65 after 64 shell reads,
  a failing cd-prefixed cargo test restarting the count, short sed write
  audit preservation, command aliases and lexical/resource limits.
- Independent first review found quoted-option concatenation, retained
  programs' output/execute flags and slash-addressed sed prints. Added
  reproductions and fixed these; final independent re-review requested.
  Workspace gates and matched performance qualification remain pending.

- Final alias follow-up: test-first `file --comp -m magic` and unquoted
  `find . -de*` reproduced wrong read verdicts. Guard abbreviated compile
  flags and all unquoted glob expansions while keeping quoted find patterns.
  Five AB1 tests pass again (`ab1-green.log`). Input/argv-bound tests include
  exactly 128 words, overflow, malformed quotes and concatenated options.
- First candidate workspace pipeline exited 0 for test/fmt/Clippy/build,
  but review changed the source during it; it is superseded evidence, not
  final qualification. Re-run gates on the committed final candidate before
  push. Matched benchmark baseline is the integrated plan head `f7b0457e`.



### 2026-10-14 — AB1 final local correctness qualification

- Code/spec independent re-review: Approve on immutable `f9fb94d6`, after
  concrete alias and glob-expansion findings were reproduced and fixed.
- Final-source workspace gates: 2,277 passed, 0 failed, 14 ignored (35
  report blocks); fmt, all-target/all-feature Clippy `-D warnings`, build
  all exit 0. Test stores used task-owned tmpfs. Raw logs under AB1 worktree
  `target/qq-perf/ab1-2026-10-14/final-candidate-gate-{0,1,2,3}.log`.
- Matched integrated baseline build from detached `f7b0457e` is live.
  Performance gate remains open: concurrent cargo/nextest work and I/O
  pressure (some avg10 24.95%, full 23.91%) prevent quiet-host acceptance.
  No runtime regression test or performance gate is waived. AB1 is local;
  no implementation PR or hosted Codex approval claimed yet.


### 2026-10-14 — second hosted plan review corrections

- Fresh #295 findings: retained Git output writers, absent ask_user prompt
  text, multiple ignored instruction roots, rendered-line continuation,
  completion censoring, progressive selection eligibility and dry-run cohorts.
  Verified against code; each receives concrete acceptance rather than a waiver.
- Instruction discovery uses the highest ignored ancestor, not only its
  worktree child; host loading supplies only workspace-root instructions.
  One search per distinct discovery root covers multi-root edits without
  a per-directory walk. Preserve raw per-run counts and audit scope coverage.
- Completion now fixes observation at end + 24 h; later finishes never
  become completed-by-cutoff. Activity metrics use terminal-by-cutoff runs.
  Exclude dry-run previews from mutation and instruction cohorts.

- Independent review added the nonignored-parent case: every below-root
  edit uses workspace-root discovery, plus each applicable highest ignored
  subtree. The AB2 fixture covers `src/AGENTS.md` above ignored `src/vendor`.
- Added a corrected first-actual-edit qualification query excluding dry-run
  previews. The original historical query remains documented as a preview-
  inclusive proxy; its 9/57 result must be reconciled before qualification.
- Executed the corrected query on schema 45: 109 editing roots, median 9,
  p90 57; original thresholds remain valid. Exact-query SQLite fixture
  measures 42 reads across a preview and excludes preview-only runs.
  Evidence: `target/qq-perf/ab-plan-2026-10-14/first-actual-edit.json`.
- Second independent review approved these corrections. `cargo test -p qq
  docs_truth -- --nocapture`: 23 passed; `git diff --check` passed. Production
  source is unchanged from the integrated head with all four workspace
  gates green; these corrections change plan/research/ledger only.

- AB1 exact `git show --output=written.txt HEAD` verdict regression passes
  with all five focused AB1 tests. Merged the reviewed parent corrections;
  production source unchanged from the locally qualified classifier.
