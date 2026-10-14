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


### 2026-10-14 — trailing reads, substitutions and explicit environments

- Hosted review identifies trailing-semicolon and print-substitution reads
  wrongly treated as work, and explicit passthrough environments that can
  mutate via process initialization. Test-first cases cover both shell/exec
  environments, safe substitutions and incomplete/mutating script exclusions.
- Nonempty requested environments must count as work without changing
  execution authority; AB6 must retain that fact through export/recount.


- Safe sed fields/flags and trailing separators are implemented without
  accepting incomplete pipelines/conjunctions or write/execute flags. Ten
  prior plus two new focused regressions pass; an additional duplicate/escaped
  environment fixture preserves the original command preview. Both independent
  source reviews approve; full integration gates follow.
- Environment inspection runs only for read candidates and does not alter
  approval input. The zero-sized ignored-element vector does not allocate
  element storage. Parsing overhead still needs matched measurement.
### 2026-10-14 — failed discovery starts and environment-aware reads

- The repeated-root gate counts every matching start, including failed
  attempts, while completed pre-edit coverage stays independent. A two-root
  failed/retried fixture corrects 2 starts/0 repeats to 3 starts/1 repeat;
  fixed-window baseline remains 108 roots, one scope/start, zero repeats.
- Qualification derives historical root role from persisted depth; a
  read-only check finds zero orphaned child runs in the fixed baseline.
- AB1's narrow sed substitutions/trailing semicolons are reads; explicit
  requested process environments disqualify reads without changing authority.
  AB6 must preserve that status. Performance and deployed observation remain open.


- Tenth docs truth passes 23 tests; formatting/diff checks pass. Independent
  review approves. failed-discovery-starts.json records the executable SQL
  fixture and live fixed-window recount; all acceptance gates remain explicit.


- Immutable integrated source 8f9c4253 passes 2,285 workspace tests,
  zero failures, 14 ignored; fmt, Clippy and build exit 0. Logs
  target/qq-perf/ab1-2026-10-14/environment-substitution-gate-{0,1,2,3}.log.
  Thirteen focused classifier fixtures pass in the workspace run; independent
  source reviews approve. Matched hot-path performance remains open.
- Proactive scope review found first-page cursor placeholders were omitted
  from start counts. The query now matches typed omitted/null values and all
  runtime placeholders with Unicode whitespace trim and ASCII case folding.
  A padded-placeholder fixture counts 14 starts across two roots with one
  repeated root; baseline remains one start. Other cursor values require
  exact prior-next provenance or leave qualification open.
  Evidence: placeholder-discovery-starts.json; independent design review.


- Placeholder docs truth passes 23 tests; formatting and diff checks pass.
  Independent final review approves typed/trimmed cursor-start semantics and
  the explicit provenance requirement for all other cursor records.


### 2026-10-14 — output-only shell reads and workspace temporaries

- Focused regressions reproduce Work for sed line-number printing/uniq
  stdout and Read for workspace sort temporary-directory options. Receipts
  extra-output-red.log and sort-temporary-red.log. Accept output-only `=`
  sed scripts and uniq's second-operand stdout sentinel; reject sort's
  temporary-directory options, including clusters and long abbreviations.
  Approval authority remains unchanged. Full integration/performance open.


- Immutable source 218cb185 passes 2,287 workspace tests, zero failures,
  14 ignored; fmt, Clippy and build exit 0. Fifteen focused classifiers pass.
  Logs: target/qq-perf/ab1-2026-10-14/output-temporary-gate-{0,1,2,3}.log.
  Latest shell-read repairs retain authority and preview behavior. Matched
  performance remains open; downstream integration is pending.
### 2026-10-14 — whole-run discovery repetition and export inputs

- Repetition counts now include starts after the first below-root edit;
  coverage retains separate successful pre-edit/per-target requirements.
  Executable fixture adds successful and failed placeholder starts after
  edits: [1,2,16,1,1,2]. Fixed baseline remains [108,1,1,0,0,1].
  Receipt: post-edit-discovery-starts.json. Independent query review agrees.
- AB6 contract requires exact runtime-envelope parsing, typed requested-env
  status and pinned exporter/recount identity. AB4 explicitly tests feature-
  enabled fetch exclusion; current unattended Network filtering already
  excludes it, so the hosted exposure claim is stale. AB2 acceptance bounds
  pre-budget directory enumeration and rejects incomplete sorted-page proof.

- Fresh parent docs-truth verification passes 23 tests after invalidating
  qq/qq-core debug artifacts; fmt/diff checks pass. No production sources
  change. Logs: whole-run-discovery-docs-truth.log. Exact SQL fixture and
  current fixed-window recount retain the baseline above.


- Fresh immutable source 2a59cfeb passes 2,287 workspace tests (0 failures,
  14 ignored), fmt, all-target/all-feature Clippy and build after explicitly
  cleaning every workspace package's debug artifacts. The source remained
  clean/unchanged throughout. Receipt: fresh-whole-run-integrated-receipt.json;
  logs fresh-whole-run-integrated-{0,1,2,3,4}.log. This source-specific receipt
  supersedes earlier shared-cache qualification uncertainty. Performance open.

- 2026-10-14 AB1 extended-regex/tree follow-up: reproduced both verdict bugs
  (15 pass, 2 fail), then admitted sed -E/-r/--regexp-extended and closed n/E/r
  short clusters while preserving write/execute rejection. tree -R is Work;
  restored ordinary -T title handling. Final focused AB1 run: 17 pass, 0 fail.
  Inherited PATH/TMPDIR effects remain outside lexical progress classification
  and do not grant approval authority. Full immutable qualification follows.
- 2026-10-14 review follow-up: corrected nearest-rank offsets for both first-edit
  queries. Deterministic empty/singleton/even/odd/duplicate fixtures pass;
  the actual-edit baseline remains 109 roots, median 9, p90 57. Alias grouping
  now requires immutable call-time evidence; current filesystem resolution
  cannot qualify historical discovery identity. Shell qualification is explicitly
  lexical_read_share; inherited executable/environment effects remain unqualified.
  Updated the sort temporary-directory and uniq stdout contracts to match AB1.

- AB1 immutable follow-up source 5f989e64dc5d12850599d00b19dd2d5753e8c18b:
  fresh workspace package-clean qualification passed 2,289 tests, 0 failures,
  14 ignored; fmt, all-target/all-feature Clippy -D warnings, workspace build
  exit 0. Receipts fresh-regex-tree-integrated-{0..4}.log and receipt.json.
  Performance qualification remains open; shared-host pressure is not waived.

- AB1 inherited-effect regression: exact sort -S1K huge-input reproduced Read
  before the fix. All sort and Cargo/rustc forms now count as Work because
  inherited temporary storage and rustup installation cannot be neutralized
  by this lexical predicate. Focused run: 18 pass, 0 fail. Retain PATH identity
  as an explicit lexical limitation; no approval authority is expanded.
- 2026-10-14 qualification follow-up: typed discovery paths cannot become
  scopes through SQLite coercion. The diagnostic now exposes invalid scopes
  and executed shell/exec/external runs with unproven mutation paths. Fixtures
  cover numeric/array/object roots and a shell-only sed edit. Historical scope
  diagnostic is 108/1/1/0/0/1, invalid scopes 0, unproven mutation runs 208;
  the whole editing cohort remains unqualified. First-edit ordering has the
  same immutable mutation-evidence requirement.

- AB1 scope correction: sort remains Work because ordinary spills can write
  through inherited TMPDIR; explicit -T rejection alone is insufficient.
  Cargo/rustc version queries are Work because rustup proxies can install
  missing workspace-selected toolchains. These are known program semantics,
  distinct from unqualified arbitrary executable identity.

- Independent parent review caught the separate mcp__ external prefix; include
  it alongside ext__ in unproven mutation evidence. A completed MCP-only
  mutation fixture now leaves the cohort unqualified too.

- AB1 final review wave immutable source 087b705d7e0847f06e6c4c8f4f28031264258955: fresh
  workspace package-clean qualification 2290 pass, 0 fail,
  14 ignored; fmt, all-target/all-feature Clippy -D warnings, workspace
  build exit 0. Receipts target/qq-perf/ab1-2026-10-14/fresh-final-review-wave-{0..4}.log and receipt.json.
  Named performance and whole-cohort observation gates remain open.

- AB1 execution-context test-first: a child test process captures PATH with a
  workspace-local executable named grep that actually writes written.txt.
  Existing literal exec classification calls it Read, so progress and audit
  assertions fail. The real helper execution and side effect are checked before
  the verdict. context-shadow-red.log retains the failing worker/parent result.

### 2026-10-14 — expanded context implementation in progress

- Inherited-PATH executable spoof reproduced red and now passes with execution-derived Work accounting. Prepared launch captures environment once, holds cwd, binds explicitly approved self-contained static Linux exec images with sealed bytes, and produces typed atomic store receipts. CLI approvals propagate through immutable factories; existing-server attachment rejects nonempty flags.
- Local source remains unqualified: focused tests identified a store-fixture table typo (fixed), broader tests exposed old heuristic assumptions and partial-migration fixtures (fixed); descriptor golden awaits refresh. No new source pushed. Integrating newly merged main #295/#296/#297 and preserving its schema46 before the next qualification.

### 2026-10-14 — current-main integration and focused context evidence

- Local merge `24653324` includes main `f87d5d9d` (#295/#296/#297). Resolved schema conflict by preserving AC3's46 and appending receipt47; AB5 must move its notice to48. GitHub still has the older head until qualified source is pushed.
- Focused AB1 run:25 tests pass,0 fail; CLI/factory run:3 pass,0 fail. Real static-image worker proves64 verified exec calls reach the stall report, replacement image/cwd paths cannot replace held inputs, and failed writes trigger audits. Receipt rollback/bounds/reopen and historical NULL fixtures pass.
- Independent read-only reviews approved host flag propagation and receipt transaction/migration wiring; executable review found and fixed unreadable-PATH skipping and unbounded sealed readback. Full workspace checks and matched performance remain open. Logs `/tmp/qq-context-{focused,root-tests}.log` (will copy into task-owned evidence before qualification).

- First fresh expanded-source workspace attempt on `1f48e28d` stopped in root tests:268 passed,2 failed,2 ignored; new CLI flag lacked its guide entry and the current descriptor assertion still expected13. Both corrected; no later gates ran. Failed logs retained under `target/qq-perf/ab1-2026-10-14/expanded-context-first-gates-*`. Independent final read-only reviews approved executable binding and receipt/ADR wiring on1f48.

- Fresh source `ea6482b7`: workspace tests2341 passed,0 failed,14 ignored; fmt passed. Clippy failed2 large-enum checks because inline process receipts enlarged `TurnStep`/`RunInput`; build not run. Receipts now allocate one boxed payload only for started processes, preserving ordinary/default event size. Cwd capability stays owned on unsupported platforms too. Failed logs retained as `expanded-context-final-gates-*`; fresh requalification follows.
