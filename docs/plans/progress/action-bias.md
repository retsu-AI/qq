# Ledger — Action bias

Plan: [`../action-bias.md`](../action-bias.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| AB1 | Shell reads after a leading `cd` are reads for progress and audit | In progress | [ENG-1038](https://linear.app/retsu-ai/issue/ENG-1038) | [#301](https://github.com/retsu-AI/qq/pull/301), `fix/eng-1038-ab1-shell-reads` | Draft; performance qualification open |
| AB2 | Prompt v20: action bias, exploration budget, built-in mapping, scoped discovery | In progress | [ENG-1039](https://linear.app/retsu-ai/issue/ENG-1039) | [#302](https://github.com/retsu-AI/qq/pull/302), `feat/eng-1039-ab2-action-prompt` | Draft; subset correction qualified, performance open |
| AB3 | `read_file` window fills the byte budget; line-limit marker | In review | [ENG-1040](https://linear.app/retsu-ai/issue/ENG-1040) | [#303](https://github.com/retsu-AI/qq/pull/303) (draft) | Latest parent integration checks in progress; performance open |
| AB4 | Declared tools and prompt match the run's authority; ADR-0059 | In review | [ENG-1041](https://linear.app/retsu-ai/issue/ENG-1041) | [#304](https://github.com/retsu-AI/qq/pull/304) (draft) | After AB2; ADR-0059 reserved |
| AB5 | Root stall report asks for the next change | In review | [ENG-1042](https://linear.app/retsu-ai/issue/ENG-1042) | [#305](https://github.com/retsu-AI/qq/pull/305) (draft) | Latest parent integration and notice docs in progress; performance open |
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
### 2026-10-14 — AB2 subset correction qualification

- Independent source review approved conditional conventions and captured
  subsets; seven focused AB2 tests and 59 prompt tests pass. Workspace gates
  pass: 2,284 tests, 0 failed, 14 ignored; fmt, Clippy and build exit 0.
  Logs: `target/qq-perf/ab2-2026-10-14/subset-final-gate-{0,1,2,3}.log`.
- Fixed prefix 5,299 bytes; builtin schema unchanged. Matched plan compilation
  performance remains unqualified; draft source review continues.

### 2026-10-14 — AB3 integrated publication qualification

- Independent final source review approved. Integrated workspace: 2,289
  passed, 0 failed, 14 ignored; fmt, Clippy and build exit 0. First attempt
  stopped when the filesystem filled; removed only task-owned obsolete
  incremental caches, then reran all gates. Logs under this worktree
  `target/qq-perf/ab3-2026-10-14/integrated-space-retry-{0,1,2,3}.log`.
- Actual parallel coverage recovers all 6,000 rows: baseline 30 calls/0 cuts,
  candidate 15 calls/4 cuts. Explicit windows, ranges and scan-cap fixtures pass.
- Loaded-host diagnostics: dispatch 94,369 → 64,433 ns/iteration (1,000);
  default output bounding 132,475 → 128,788 ns. Matched/noise performance
  qualification remains open; these samples do not establish a speedup.
- Draft source publication next, stacked on AB2; no readiness claim.

### 2026-10-14 — AB3 continuation start-offset boundary

- Hosted plan review supplied a 100,000-line start-offset boundary. A
  105,000-short-line fixture starting at 99,999 reproduces an unusable
  next offset 103,227. Test-first fails before the correction. Preserve
  the existing bound and emit a terminal marker naming it instead.
### 2026-10-14 — AB2 shell-only and range guidance regressions

- Hosted review found absent companion tools in shell-only descriptions and
  read_file-specific ranges in no-tools prompts. Extend captured subsets
  with shell-only interactive requests and description/ranges assertions.
  Test-first fails on no-tools range guidance before production correction.
- Independent final review approved conditional ranges and compiled
  descriptions. Seven focused tests pass, including shell-only/edit-only
  captured profiles after the common-path allocation adjustment. Full
  builtin schema remains unchanged; workspace requalification follows.
### 2026-10-14 — fresh AB1 helper regressions

- Latest hosted source review supplied signature-format verification,
  compressed ripgrep helpers, configured Cargo compilers, repeated sed
  expressions and JJ external formatters. Exact test-first table fails
  before fixes; independent investigation confirms configuration defaults
  can execute helpers, and bare JJ commands snapshot the working copy.
- Narrowing canonical safe Git/JJ forms and treating Cargo queries as work
  is required; no read verdict or approval authority is inferred from hints.
- Independent final review approved helper narrowing; all five focused
  regressions pass. Workspace requalification and fresh candidate build next.

### 2026-10-14 — fifth hosted plan review corrections

- Added explicit find file-list output and sort compressor exclusions,
  inspection-only interactive authority, and terminal read offset bounds.
  AB1/AB3/AB4 source regressions reproduce the corresponding cases.
- Preview exclusion now tests JSON boolean true only and handles malformed
  JSON explicitly. Exact-query fixture retains four failures/five calls
  while excluding the actual preview; corrected fixed-window baseline remains
  205/1,222. Evidence `target/qq-perf/ab-plan-2026-10-14/malformed-preview.json`.
- Updated concrete helper rules from fresh AB1 review; independent final
  review and documentation checks pending. No gates waived.
- Independent review approved all five plan corrections. Docs truth 23
  passed; exact malformed-preview fixture and baseline recount passed.
  Production parent remains unchanged from its qualified source.


- Latest helper source workspace gates pass: 2,277 tests, 0 failed,
  14 ignored; fmt, Clippy and build exit 0. Integrated parent docs only;
  logs `target/qq-perf/ab1-2026-10-14/configured-helper-gate-{0,1,2,3}.log`.

### 2026-10-14 — AB5 replay-stable root notice start

- Isolated branch `feat/eng-1042-ab5-root-stall-action`, AB4 parent604dc6a6.
  User-authorized stack; parent final qualification remains open. Test-first
  root notice mapping/live roots and schema46 migration/reopen next.
- Independent read-only investigation mapped root/child/retry fixtures and
  historical migration skips. Pin a schema45 test binary before the bump
  and verify actual file opening refuses46, without per-session decoding.

### 2026-10-14 — AB5 root notice and compatibility evidence

- Test-first mapping, live-root and migration assertions reproduced the
  missing behavior. Root policy now emits RootStallReport; child and legacy
  notices retain their original text. Pinned retries and delivery decoding
  recognize both report variants; unknown persisted values remain errors.
- Schema46 adds a version-only migration. The pinned schema45 probe binary
  (SHA256 7723fe8faa0b05ad9ee421e705f37cb283fc26cd0bb59133de965a7bd4be732e)
  refuses an actual schema46 store at open with CONSTRAINT, leaving46 intact.
- Five focused AB5 tests pass, including live read-only root, persisted
  follow-up, independent reference assembly and byte-identical legacy replay.
  Historical migration and child progress tests pass in the focused run.
  Logs: target/qq-perf/ab5-2026-10-14/{focused-final,replay-focused,progress-complete-decode}.log.
- Independent production-source review approves. Parent integration, full
  immutable workspace gates, final measurements and draft publication remain
  open; no readiness or rollout acceptance is claimed.

- Initial immutable workspace gate found one stale schema45 constant assertion
  in the side-question migration fixture (980 core tests passed, one failed).
  Verify actual reopened metadata equals the current schema instead; rerun
  workspace gates on the corrected commit.

- Corrected immutable source05044927 passes full workspace gates: 2,301 tests,
  zero failed,14 ignored; fmt, Clippy and build exit0. Logs
  target/qq-perf/ab5-2026-10-14/root-notice-final-gate-{0,1,2,3}.log.
  This qualifies the local slice against its current parent only; newer
  parent integration, measurement and publication remain open.
### 2026-10-14 — AB3 long-header regression

- Hosted source review supplied a valid 615-byte path. Fixed 256-byte
  header reservation allows generic bounding to drop middle rows while
  reporting contiguous offsets. The model-visible row-contiguity fixture
  fails before correction; budget the rendered header before the body.
- Rendered header upper bound now reserves actual escaped path and metadata
  bytes plus header rows. All seven focused AB3 tests pass; independent
  final review approved. Workspace requalification follows.

- Retained the historical 256-byte minimum reservation while reserving the
  full rendered header for longer paths. This preserves ordinary read-window
  sizes and the existing replay-budget fixture without weakening bounds.
  Seven focused AB3 regressions and the exact replay fixture pass; independent
  final review approves this compatibility adjustment. Full workspace rerun
  remains open, along with fresh scan-cap review and matched performance.

- Fresh hosted scan-cap claim checked against current source: the exhaustive
  5 MiB fixture follows every advertised offset successfully until the
  existing terminal scan-cap marker. A single over-cap line is also terminal.
  Regression passes without a production change; the older draft contract
  needs to state this existing terminal behavior explicitly.

- Immutable source0eaf8dd2 workspace gates pass: 2,292 tests, zero failed,
  14 ignored; fmt, Clippy and build all exit0. Logs
  target/qq-perf/ab3-2026-10-14/header-final-gate-{0,1,2,3}.log.
  Matched performance and newer upstream integration remain open.

- Integrated qualified AB3 read-window source837786c9 (merge b1a61680).
  Added captured direct requests where an external host changes its grants
  after compilation, testing both grant-removal and grant-addition directions.
  Execution and declaration remain tied to the compiled snapshot; focused
  regression passes without a production change. Final parent integration
  and immutable workspace gates remain open.
- Integrated description/range correction workspace gates pass: 2,284
  tests, 0 failed, 14 ignored; fmt, Clippy and build exit 0. Logs
  `target/qq-perf/ab2-2026-10-14/description-final-gate-{0,1,2,3}.log`.
  Source and full builtin schema are independently reviewed; performance open.

### 2026-10-14 — AB2 child, ignored-walk and exec-only guidance

- Three test-first assertions reproduced read-child edit demand, incomplete
  ignored-path guidance and exec-only absent-shell references. Read children
  now return findings; ignored search/tree calls require path plus
  include_ignored=true in prompts and declarations; exec-only descriptions
  preserve literal arguments and cleared-environment semantics.
- Eight focused AB2 tests and61 prompt-filtered tests pass. Independent
  final review approves the prompt/compiler correction; declaration wording
  is covered by captured search/tree requests. Full input schemas are unchanged;
  corrected descriptions change canonical full tool identity to
  f34eb03af41070290dc4ee6ea49d4c12b011cc409b823e795beaa92cc9c453ed.
  Full fixture prefix5342bytes. Historical prompt14 golden remains unchanged.
- New symlink/depth discovery findings and upstream integration remain open;
  workspace qualification and matched performance follow immutable final source.

- Correction to the prior prompt-filter receipt:61 passed before updating
  the two ignored-walk declarations. The next run was60 passed,one failed:
  its canonical tool-identity assertion retained the old description hash.
  Update that assertion to the measured f34eb03a… identity; historical
  prompt14 digest itself still passed unchanged. Final rerun follows.

### 2026-10-14 — AB2 bounded instruction discovery completion

- Test-first safe-link and depth fixtures reproduced omission; names searches
  now report regular-file symlinks resolved inside the workspace. Content
  searches and directory traversal retain link exclusion. Matching broken,
  escaping or nonfile instruction links return a blocker.
- Depth omissions are explicit (partial=depth, or depth_limited=64 alongside
  another stop); targeted ancestor roots at each64-component boundary cover
  deep edits. No unusable depth cursor is emitted.
- Independent review caught a charged nonmatching-link cursor gap. The public
  50,000-link fixture reproduced it; every considered link now advances the
  scan watermark. Paginated matching links, cancellation and incomplete hint
  counts retain their contracts.
- Twelve focused AB2 tests pass; independent final review approves. Fixture
  prefix5728bytes; canonical full schema identity remains f34eb03a… after
  the earlier declaration corrections. Historical prompt14 remains pinned.
  Logs instruction-discovery-{red,final}.log, symlink-budget-red.log and
  depth-hint-red.log under target/qq-perf/ab2-2026-10-14.
- Full workspace gates, upstream integration and fresh matched performance
  remain open. Parent plan qualification corrections are being reconciled.

### 2026-10-14 — AB1 configured environment and native shell corrections

- Test-first fixture reproduced the configured-Git false read. Git reads now
  require canonical -c core.fsmonitor=false after optional-C; ripgrep reads
  require --no-config. fd detailed listings and file decompression count work.
- Unix quoted regex metacharacters are literal; unquoted metacharacters and
  expansion inside double quotes remain work. Windows single quotes, expansion
  and escape characters and native sort are conservative work. Windows-only
  fixtures are present but have not been executed on this Linux host.
- Six Linux focused regressions pass; independent final source review approves.
  Full workspace rerun and fresh matched performance remain open.
  Evidence target/qq-perf/ab1-2026-10-14/environment-helpers-{red,focused-final}.log.

- Initial full rerun found one existing live progress fixture using plain
  git status, now intentionally work because of configured fsmonitor. Update
  its read case to canonical git -c core.fsmonitor=false status; keep its
  report assertion. Core957 passed,one failed,three ignored before correction.

- Corrected immutable sourcebb106e3f passes workspace gates: 2,278 tests,
  zero failed,14ignored; fmt, Clippy and build exit0. Logs
  target/qq-perf/ab1-2026-10-14/environment-helpers-final-gate-{0,1,2,3}.log.
  Windows-only execution and final matched performance remain open.

- Initial immutable integrated workspace run:969 core tests passed,one failed,
  three ignored. The remaining failure was the second builtin declaration
  golden retaining the old description identity. Update it to measured
  f34eb03a…; schemas remain unchanged. Full immutable gates rerun next.

- Integrated current AB2 bounded instruction discovery and helper-qualified
  AB1 source. Declaration identity now combines AB2 corrected descriptions
  with AB3's removed default200: a8f52f3e1dc6398001a13aea54ccfeb8624c167a3c20378adcb9b9f50de42323.
  Keep both corresponding golden assertions in sync with the measured full
  catalog. Parent source integration requires fresh immutable workspace gates.
- Immutable integrated sourcec7c0bc41 passes workspace gates:2,290 tests,
  zero failed,14ignored; fmt, Clippy and build exit0. Logs
  target/qq-perf/ab2-2026-10-14/discovery-qualified-gate-{0,1,2,3}.log.
  Newer parent documentation integration and matched performance remain open.
### 2026-10-14 — sixth hosted plan review corrections

- Explicit existing classifier exclusions now cover find okdir, fd exec/
  exec-batch, tree output and file compile modes. No runtime verdicts
  changed for these cases. Preview exclusion applies only to edit_file;
  write_file dry_run is a schema failure. Corrected205/1,222 baseline is
  published beside the obsolete211/1,230 proxy.
- Successful discovery starts must precede actual edits in a model-visible
  earlier turn. Same-turn/failed/later starts cannot prove coverage; later
  new targets require separate ordered coverage evidence. Exact-query
  fixtures pass: five failures/six nonpreview calls; one eligible discovery
  of four starts. Fixed historical early scope:108roots,18distinct roots,
  25starts,7repeated roots,5affected runs,max5. Coverage remains unqualified.
- Independent final review approved; docs truth/fmt/diff checks next.
  Evidence `target/qq-perf/ab-plan-2026-10-14/typed-preview-preedit.json`.
- Docs truth23passed; fmt/diff checks pass. Production parent unchanged
  from qualified source; sixth-review corrections ready for hosted review.

### 2026-10-14 — seventh hosted plan review reconciliation

- Clarified canonical Git/ripgrep configuration disables, native Windows
  uncertainty, read-child findings, ignored-walk overrides, safe instruction
  links/depth windows, terminal scan-cap markers and compiled grant lifetime.
  Source regressions and independent reviews cover the corresponding slices.
- Export overflow makes qualification unqualified unless the pinned classifier
  itself rejects the command. Canonical persisted read identity replaces
  argument spelling for re-read qualification; unresolved aliases remain open.
- Exact SQL fixture retains a failed boolean-true preview (one failure/two
  attempts) and accepts discovery between root-level and below-root edits.
  Corrected fixed-window baseline:211 failures/1,228 attempts,17.1824%;
  below-root early scopes108/18/25/7/5/5,still unqualified for path/page/read
  coverage. Evidence completed-preview-below-root.json under the plan receipts.
- Independent final plan review approves. Docs truth23 passed; fmt and diff
  checks passed. Parent production source is unchanged from its qualified
  source; no acceptance or performance gate is waived.

- Integrated immutable AB4 workspace gates pass:2,305 tests,zero failed,
  14ignored; fmt, Clippy and build exit0. Logs
  target/qq-perf/ab4-2026-10-14/authority-qualified-gate-{0,1,2,3}.log.
- Integrated immutable AB3 workspace gates pass:2,298 tests,zero failed,
  14ignored; fmt, Clippy and build exit0. Logs
  target/qq-perf/ab3-2026-10-14/discovery-qualified-gate-{0,1,2,3}.log.
  Independent source/integration review approves. Matched performance and
  hosted review remain open; publish as draft without a readiness claim.

- AB4 published as draft PR304, stacked on303; LinearENG-1041 In Review.
  Hosted review requested. Parent source0dc6f436 receipt integrated; no
  production change after full2305-test qualification. Matched performance
  remains open; no merge or readiness claim.

- Immutable integrated source185f983e passes full workspace gates:2,310 tests,
  zero failed,14ignored; fmt, Clippy and build exit0. Logs
  target/qq-perf/ab5-2026-10-14/integrated-root-notice-gate-{0,1,2,3}.log.
  NO_COLOR removed, task-owned tmpfs temporaries, incremental off; dev/test
  debug symbols disabled to bound task-owned build storage (debug assertions
  retained). These are correctness gates, not performance measurements.
- Integrated AB4 draft304 parent receipt with no production change. Matched
  performance and hosted review remain open; publish AB5 as draft.

- Fresh parent review alleged stale grants in uncompiled Runtime::run_messages.
  Current AB4 already replaces its empty placeholder gate after compilation
  using the compiled descriptor snapshot. Public first-poll regression now
  tests grant addition/removal between stream construction and polling; both
  declarations and execution follow the newly compiled grants. Focused pass
  requires no production change. Rerun workspace before publishing the test.

- Immutable first-poll regression source 5759c682 passes all workspace gates:
  2,306 tests, zero failed, 14 ignored; fmt, Clippy and build exit 0. Logs
  target/qq-perf/ab4-2026-10-14/uncompiled-grants-gate-{0,1,2,3}.log.
  Declarations and execution use grants compiled at first poll; no production
  change was needed. Hosted review and matched performance remain open.

### 2026-10-14 — AB2 file ignores, empty directories and write-only contract

- Test-first public discovery fixtures reproduce hidden instruction basenames
  in visible directories and no resumable cursor after 50,000 empty dirs:
  12 focused tests pass, two fail. Captured write-only request separately
  reproduces unavailable prior-read guidance. Red receipts under AB2 target.
- Repair preserves directory ignores and names-only safe-link handling; only
  fully visited directories may advance a scan watermark. Work in progress.

- Initial repairs pass 14 focused fixtures. Independent review found a depth
  skip could suppress later empty-directory watermarks; new public fixture
  reproduces it (one failed). A per-subtree depth-skip counter now permits
  only completed subtrees to advance. Ignored instruction links are covered
  alongside regular files; write-only captured declarations name creation
  and an independently known current hash, without unavailable prior reads.

### 2026-10-14 — configured Git helpers and literal exec regressions

- New hosted findings reproduced test-first: working-tree filters, signature
  branch formatting/default sort and remerge helpers must count as work.
- Exact exec argv containing spaces, quotes and shell metacharacters is a
  literal read, independent of native shell quoting. Focused red run: six
  pass, two fail; configured-git-exact-exec-red.log in AB1 receipts.
- Repair and independent read-only review in progress; performance remains open.

- Repair caches one progress/audit verdict from exact exec argv or checked shell
  syntax. Working-tree Git forms count as work; branch reads force refname
  sorting, and history reads force no patch/merge-diff generation. Approval
  previews, grants and decisions are unchanged. Initial focused green: eight
  tests passed; independent final review and immutable workspace gates next.

- Immutable source 4bd16392 passes workspace checks: 2,280 tests, zero failed,
  14 ignored; fmt, Clippy and build exit 0. Logs
  target/qq-perf/ab1-2026-10-14/configured-git-exact-exec-gate-{0,1,2,3}.log.
  Independent final read-only review approves; eight focused regressions pass.
  Matched performance remains open; no readiness claim.

### 2026-10-14 — eighth hosted plan review reconciliation

- Corrected Goal scope baseline to 25 starts, seven repeated roots, five runs;
  raw argument counts remain separate and coverage remains unqualified.
- Candidate begins after pinned AB1–AB5 main deployment and prior-writer
  retirement, with runtime source/binary and continuity proof. Merge dates
  and prompt version alone cannot qualify treatment. No deployment performed.
- Public uncompiled first-poll grant regression is qualified in AB4; AB1
  helper/exec and AB2 ignored-file/empty-directory repairs are in progress.

- Independent eighth plan review approves; docs truth 23 passed. Source
  qualification remains attached to the respective slice receipts; no
  performance or deployed observation gate is waived.

- Integrated immutable source e1e56e7a passes all workspace checks: 2,295
  tests, zero failures, 14 ignored; formatting, Clippy and build exit 0. Logs
  target/qq-perf/ab2-2026-10-14/ignored-files-empty-dirs-gate-{0,1,2,3}.log.
  Fifteen focused AB2 fixtures pass; independent final review approves the
  depth/empty-directory continuation fix, instruction file-ignore handling
  and write-only contract. Matched performance and hosted review remain open.

- Integrated accepted AB1 helper/exec fixes and AB2 discovery fixes with their
  qualified receipts. AB3 production read-window behavior is unchanged; rerun
  immutable workspace gates on the combined source before updating draft #303.

- Integrated accepted AB1 exact-argv/Git-helper and AB2 instruction discovery
  repairs through the AB3 parent. AB4 authority implementation is unchanged.
  Rerun immutable combined workspace gates; hosted review and performance open.

### 2026-10-14 — AB5 notice documentation regression

- Hosted review found architecture's stored-notice list and report-query
  runbook omit root_stall_report. New docs-truth regression fails before
  correction (notice-docs-red.log); shared paths reserved in root ledger.
- Correct the report vocabulary and all-report predicate, preserving Goal 6's
  slice-only selection explicitly. Exact SQL fixture and full gates next.

- Architecture now names root_stall_report and child/legacy stall_report.
  Runbook defaults to all three report kinds on schema 40+, with schema 46
  semantics explicit; Goal 6 uses its narrower slice-only predicate.
- Docs truth 24 passed. Exact SQL fixture: all reports 3/missed 1; Goal 6
  slice reports 1/missed 0. Evidence notice-query-fixture.json in AB5 receipts.
  Independent documentation/integration review approves; full gates next.
