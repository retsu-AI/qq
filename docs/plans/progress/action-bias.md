# Ledger — Action bias

Plan: [`../action-bias.md`](../action-bias.md). Only the agent working this
plan edits this file. Current state on top; dated entries appended below,
newest last.

| Slice | Goal | Status | Linear | Branch / PR | Notes |
| --- | --- | --- | --- | --- | --- |
| AB1 | Shell reads after a leading `cd` are reads for progress and audit | Planned | [ENG-1038](https://linear.app/retsu-ai/issue/ENG-1038) | | Start after AP5's window (closed 2026-10-13 17:42 UTC) |
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
