# Ledger — Action bias

Plan: [`../action-bias.md`](../action-bias.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| AB1 | Shell reads after a leading `cd` are reads for progress and audit | In progress | [ENG-1038](https://linear.app/retsu-ai/issue/ENG-1038) | | Start after AP5's window (closed 2026-10-13 17:42 UTC) |
| AB2 | Prompt v20: action bias, exploration budget, built-in mapping, scoped discovery | In progress | [ENG-1039](https://linear.app/retsu-ai/issue/ENG-1039) | [#302](https://github.com/retsu-AI/qq/pull/302), `feat/eng-1039-ab2-action-prompt` | Draft; performance qualification open |
| AB3 | `read_file` window fills the byte budget; line-limit marker | In progress | [ENG-1040](https://linear.app/retsu-ai/issue/ENG-1040) | `perf/eng-1040-ab3-read-window` | Correctness locally qualified; performance open |
| AB4 | Declared tools and prompt match the run's authority; ADR-0059 | In progress | [ENG-1041](https://linear.app/retsu-ai/issue/ENG-1041) | | After AB2; ADR-0059 reserved |
| AB5 | Root stall report asks for the next change | Planned | [ENG-1042](https://linear.app/retsu-ai/issue/ENG-1042) | | After AB1 |
| AB6 | Seven-day re-measure; paired run if ENG-809 authorizes | Planned | [ENG-1043](https://linear.app/retsu-ai/issue/ENG-1043) | | After AB6.1 acceptance and AB1–AB5 + 7 days |

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


### 2026-10-14 — AB2 baseline and regression start

- Branch `feat/eng-1039-ab2-action-prompt` stacks on AB1 `2d0bfa4f`.
  Pre-change `cargo bench -p qq-core --bench plan_compile`: 29,242 ns per
  compile (2,000 iterations), descriptor digest 2,562 ns (20,000), estimated
  plan bytes 34,963, canonical descriptor bytes 1,408. Fixture stores on
  tmpfs, shared host; final paired/noise qualification remains open. Raw
  baseline: AB1 worktree `target/qq-perf/ab2-2026-10-14/baseline.log`.
- Added root contract, read-child and absent-tool header regressions before
  production changes, plus explicit ignored-worktree names/content searches.
  Fixed-path fixture prints prefix bytes and schema hash for before/after.

- Test-first baseline: four focused AB2 tests, 2 passed / 2 failed. Root
  action contract and absent-tool header reproduce the intended defects;
  read-child section and explicit ignored-worktree names/content searches
  already pass. Fixed prompt fixture: 4,214 bytes; built-in schema hash
  `9ecb4e7efbcc1de6556cc22803ea7579df287b71f70bf4aaf79ec4a81d69a4cb`.
  Raw test evidence: `target/qq-perf/ab2-2026-10-14/red-compiled.log`.

### 2026-10-14 — AB2 implementation review

- Implemented prompt v20 and built-in mappings on AB1 `2d0bfa4f`; independent
  review approved after adding both ignored and nonignored ancestor scopes.
- `cargo test -p qq-core ab2_ -- --nocapture`: 5 passed; `cargo test -p
  qq-core prompt -- --nocapture`: 58 passed. First fixture attempt failed
  because its TMPDIR parent was missing; created the task-owned directory
  and reran successfully. Prefix fixture: 5,516 bytes; descriptor schema
  hash `f461f1c99f8dda8b83ae9289c8cfba92ccd3d27d724c9225de2d5063e4f02a85`.
- Baseline plan compilation: 29,242 ns/iteration (2,000 iterations), digest
  2,562 ns/iteration (20,000); candidate and noise qualification remain open.
  Workspace gates are next; nothing in this slice has been pushed.
- First workspace gate found one stale headless JSONL prompt-version
  assertion (expected 19, observed 20); reserved its test-only path in the
  root ledger and updated the expected identity. Gate rerun remains pending.
- Corrected headless regression passes (1 test). Thirty alternating A/B
  and same-binary A/A pairs recorded in `paired-plan-compile.json`: paired
  compile median +2.16%, A/A +0.82% (pair range -4.54% to +7.64%). Concurrent
  builds invalidate quiet-host qualification; these are diagnostic samples,
  not a waived performance gate. Initial candidate 30,239 ns/iteration.
- Second full test run exposed four more stale contract assertions: plan
  digest, tool-schema digest and two delegation prompts. Updated their
  expected v20 identities/text, including the negative read-child assertion;
  no production behavior was changed to satisfy them. Requalification next.

### 2026-10-14 — AB3 test-first start

- Isolated `perf/eng-1040-ab3-read-window` stacks on AB2 `ad32760e`.
  Pre-change benchmarks recorded in AB2 worktree `ab3-2026-10-14`: dispatch
  94,369 ns/iteration, output fits 2,153 ns/iteration, shell bound 24,414.
  Shared-host diagnostic; final matched qualification remains required.
- Added public read-window, 4,000-line contiguous continuation and six-result
  turn-budget fixtures before production changes. Red run pending.
- Pre-change focused tests: 1 passed, 2 failed (default stops at 200 and
  line-stop has no offset marker). Six-result fixture baseline: 30 reads,
  0 turn-budget cuts, mean 7,079 bytes per read. This is deterministic output
  projection characterization, not timing or live parallel execution.
- Scan-cap exact/unknown-count regression fails before its marker fix.
  Added an actual six-parallel-read runtime fixture that tracks only visible
  rows and recovers missing offsets; baseline runtime execution is next.

### 2026-10-14 — AB3 implementation review

- Five focused regressions pass. Actual parallel runtime recovers all 6,000
  lines: baseline 30 calls / 0 cuts → candidate 15 calls / 4 cuts; cuts buy
  fewer total calls, including recovery. Projection diagnostic 30/0 → 12/3.
- No-limit reads use EOF windows bounded by bytes and rendered lines, reserve
  header/marker rows, and emit explicit-limit continuation. Scan-capped reads
  report an unknown count without reading beyond 4 MiB. Ranges retain intervals.
- Independent code/spec review approved. Schema hash changes to
  `d0a311aab69d74e7cf7684ebefc25a1fa366a81217c529947d3b17b357e7fa0f`.
  Workspace and final benchmark gates remain open; no implementation pushed.


- Third workspace attempt passed core behavior but failed five TUI ANSI
  assertions under inherited `NO_COLOR=1`. The same compiled binary passes
  all render tests (6) and highlighted-frame test (1) with NO_COLOR removed.
  Re-run full gates with that test environment; no TUI code change needed.
- Final v20 source before basename follow-up: 2,282 passed, 0 failed,
  14 ignored; fmt/Clippy/build exit 0. Independent review approved identities
  and child contracts. Third hosted plan review requires exact-basename
  instruction search; added NOTAGENTS.md rejection, requalification next.
- Basename full gate hit unchanged MCP cancellation timing: 49.56 ms
  against 45 ms fixture bound during concurrent builds. Same compiled test
  passes alone in 20 ms; no MCP code or threshold changed. Full rerun next.


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

### 2026-10-14 — AB4 execution eligibility investigation

- Independent source investigation confirms external read-only hints grant
  nothing. Strict ReadOnly denies all External calls, including granted ones;
  unattended Ask allows only configuration-granted externals. Clarified the
  plan to preserve that gate behavior, avoid request-time grant resolution,
  and filter progressive index/selection with the declaration predicate.

### 2026-10-14 — third hosted plan review corrections

- Five fresh concrete findings: goal still capped all scopes at one search,
  sed file-supplied programs, direct-run guide omitted granted externals,
  suffix regex matched NOTAGENTS.md, and scan-cap remainder counts unknown.
  Aligned goals/contracts and added exact regression acceptance; no waiver.
- Independent review approved all five corrections and the external-hints
  eligibility clarification. Docs truth 23 passed; fmt/diff checks passed.
  AB1 exact file-supplied sed regressions pass (existing classifier rejects
  them); AB2 exact-basename fixture passes; AB3 unknown-count fixture passes.

- Exact-basename AB2 source workspace gates passed: 2,282 tests, 0 failed,
  14 ignored; fmt, all-target/all-feature Clippy and build exit 0. Logs:
  `target/qq-perf/ab2-2026-10-14/candidate-basename-final-gate-{0,1,2,3}.log`.
  Integrated latest AB1 and reviewed parent docs; final publication gates next.
- Integrated AB3 workspace tests and fmt passed. Clippy caught one nested
  `if let` in the test provider; collapsed it without changing coverage logic.
  Focused rerun and remaining lint/build gates follow.


### 2026-10-14 — AB1 draft publication qualification

- Integrated reviewed parent `188b69ff`; exact git-output and sed-file
  regressions pass without changing the classifier. Final workspace gates
  pass: 2,277 tests, 0 failures, 14 ignored; fmt, Clippy and build exit 0.
  `cargo test --workspace -- --test-threads=1` serializes fixtures while their
  internal async concurrency tests still execute. Color enabled, tmpfs stores.
- Parallel publication attempt hit unchanged MCP cancellation timing
  (45.83 ms against 45 ms); full serial rerun passes without threshold edits.
  Logs `target/qq-perf/ab1-2026-10-14/publish-serial-gate-{0,1,2,3}.log`.
- Publish as draft for hosted source review; matched performance qualification
  remains open. No readiness or merge claim is made by draft publication.
- Draft implementation PR [#301](https://github.com/retsu-AI/qq/pull/301)
  opened on `83f9621e`, base #295; requested hosted Codex review. Linear
  remains In Progress because matched performance qualification is open.

### 2026-10-14 — AB2 draft publication qualification

- Integrated AB1 and reviewed parent corrections. Workspace gates on the
  exact production source pass: 2,282 tests, 0 failed, 14 ignored; fmt,
  all-target/all-feature Clippy and build exit 0. Logs under
  `target/qq-perf/ab2-2026-10-14/publish-gate-{0,1,2,3}.log`.
- Final fixed-prefix fixture: 5,521 bytes (baseline 4,214), builtin schema
  `f461f1c99f8dda8b83ae9289c8cfba92ccd3d27d724c9225de2d5063e4f02a85`.
  Independent review approved. Draft publication awaits hosted source review;
  plan_compile quiet-host/noise qualification remains open.

- Draft [#302](https://github.com/retsu-AI/qq/pull/302) opened on `90c6b88d`,
  stacked on #301. Fourth plan review identified fixed body conventions for
  omitted tools. Captured read_file-only/no-tools regression fails before
  correction (no-tools prompt still names read_file); test-first followup.
- Custom-subset fix now renders builtin/file/command/instruction/recall
  conventions from declared tools. Captured no-tools and read_file-only
  requests pass; prompt tests 59 passed. Historical v14 prompt digest remains
  unchanged after reversing only the reviewed v20 changes. Prefix fixture
  5,299 bytes; schema unchanged. Single-tool matrix and final gates next.

### 2026-10-14 — AB1 hosted source review regressions

- #301 raised ripgrep hostname helpers, Git external diff/textconv helpers,
  and Cargo lockfile-generating queries. Added exact verdict regressions
  before changing the recognizer; independent investigation requested.
- Independent review caught Git disabling flags used as path/value operands
  and default blame textconv. Require canonical leading disable switches,
  suppress signature helpers for log/show, and test separators/overrides.
  Cargo's lock guard is a leading switch, with config overrides rejected.
  All five focused regressions pass; final independent re-review requested.
### 2026-10-14 — fourth hosted plan review corrections

- Six concrete contract gaps: sed substitution flags, sort abbreviations,
  classifier-mismatched shell metric, unattended prompt authority, mixed AB5
  measurement window and custom-subset tool conventions. Plan corrections
  require shared-classifier recount, fixed AB1–AB5 candidate window, direct
  authority wording and captured arbitrary-subset prompts. Source followups
  are test-first; no findings or performance gates are waived.
- AB6.1 is a concrete prerequisite: consistent read-only cohort export and
  bounded streamed recount with the actual AB1 classifier. Both windows use
  one pinned binary; old shell-program SQL figures stay historical proxies.
  Baseline recount and candidate qualification remain open.
- Independent final review approved the corrections and fixed-runtime paid
  comparator contract. Docs truth: 23 passed; fmt/diff checks passed.
  Production source unchanged from the previously qualified parent.

### 2026-10-14 — AB1 helper fix qualification

- Independent final source review approved. Workspace tests: 2,277 passed,
  0 failed, 14 ignored; fmt, all-target/all-feature Clippy and build exit 0.
  Logs: `target/qq-perf/ab1-2026-10-14/helper-final-gate-{0,1,2,3}.log`.
  Integrated parent docs only; production source remains qualified.
- Matched performance qualification remains open; prior candidate binary
  predates the helper fix and cannot qualify this head.

### 2026-10-14 — AB4 baseline and test-first start

- Isolated branch `fix/eng-1041-ab4-run-authority`, AB3 parent `93cef811`.
  Loaded-host baseline catalog compile 664,763 ns/500 iterations, selection
  18,393 ns/10,000, host plan compile 58,587 ns/500; not qualification.
  Raw baseline lives in AB3 `target/qq-perf/ab4-2026-10-14/baseline-catalog.log`.
- Both new regressions fail: direct request declares denied edit_file;
  strict ReadOnly retains a hinted external tool. Test-first log under
  this worktree `target/qq-perf/ab4-2026-10-14/test-first.log`.
- Snapshot eligibility/gates and prefix authority implementation next.

### 2026-10-14 — AB4 implementation review

- Shared immutable grant eligibility now filters declarations, recovered
  pins, progressive indexes and selector reports. Direct gates use the
  compiler snapshot; strict ReadOnly omits all externals. Root authority
  follows read-only/unattended flags and derived callable mutation capability.
- Eight focused AB4 tests pass; independent source review approved after
  fixing an inherited recall fast-path omission (test-first reproduced).
  Core library: 975 passed, 0 failed, 3 ignored. An earlier run stalled in
  overflow recovery; that exact fixture passes alone and the full immutable
  rerun passes. Logs `target/qq-perf/ab4-2026-10-14/core-immutable-retry.log`.
- ADR-0059 proposed, design/guide corrected. Integrate newest parent fixes
  before workspace/performance qualification and draft publication.
