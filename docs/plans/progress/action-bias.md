# Ledger — Action bias

Plan: [`../action-bias.md`](../action-bias.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| AB1 | Shell reads after a leading `cd` are reads for progress and audit | In progress | [ENG-1038](https://linear.app/retsu-ai/issue/ENG-1038) | [#301](https://github.com/retsu-AI/qq/pull/301), `fix/eng-1038-ab1-shell-reads` | Draft source review; performance gate open |
| AB2 | Prompt v20: action bias, exploration budget, built-in mapping, one instruction search | Planned | [ENG-1039](https://linear.app/retsu-ai/issue/ENG-1039) | | |
| AB3 | `read_file` window fills the byte budget; line-limit marker | Planned | [ENG-1040](https://linear.app/retsu-ai/issue/ENG-1040) | | After AB2 |
| AB4 | Declared tools and prompt match the run's authority; ADR-0059 | Planned | [ENG-1041](https://linear.app/retsu-ai/issue/ENG-1041) | | After AB2; ADR-0059 reserved |
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

### 2026-10-14 — lazy fetch, operation merge and timestamp regressions

- Test-first hosted regressions reproduce Git history lazy fetching, JJ
  operation reconciliation and file's explicit timestamp restoration as false
  reads. The same fixture requires Unix sed's literal last-line address.
- Git 2.54 locally supports --no-lazy-fetch. Official Git/JJ/file documentation
  confirms the side effects. Constrain canonical Git reads, classify JJ as
  work, and exclude timestamp-restoring file modes; approval unchanged.

- Nine focused fixtures pass after lazy-fetch/operation/timestamp repairs.
  Additional test-first Git --help case reproduces a false read: repository
  help settings may launch a browser/helper. rev-parse and worktree listing
  now use closed suffixes. Receipt git-help-red.log; final focused run next.

### 2026-10-14 — ninth hosted plan review reconciliation

- Instruction-scope qualification now requires the exact anchored basename
  regex and JSON boolean regex=true. The fixed-window recount is 108 roots,
  one scope/start, zero repeats; the 25-start substring proxy remains context.
  The deterministic query fixture rejects NOTAGENTS, mistyped regex flags,
  literal searches and continuation pages; anchored-scope-query.json.
- AB1 acceptance requires --no-lazy-fetch, closed Git inspection forms,
  JJ classified as work and file preserve-date excluded. AB4 records version
  21; historical 19/20 are AB2 treatment labels on the same accepted runtime.
- Coverage, matched performance, final classifier recount and deployed
  candidate observation remain open; no deployment or paid calls performed.

- Docs truth passes 23 tests; formatting and diff checks pass. Independent
  review caught and corrected the branch-list acceptance example's missing
  no-lazy-fetch switch. Source is unchanged; slice-specific source gates
  remain tracked independently.


- Immutable integrated source 007d6728 passes 2,282 workspace tests,
  zero failures, 14 ignored; fmt, Clippy and build exit 0. Logs
  target/qq-perf/ab1-2026-10-14/lazy-operation-gate-{0,1,2,3}.log.
  Ten focused classifier fixtures pass; independent source review approves.
  Matched performance and final hosted review remain open.
