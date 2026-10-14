# Action Bias: runs that produce results

## Status

| | |
| --- | --- |
| Now | Proposed 2026-10-13. AB1 and AB2 are independent and can start at once; AB3 and AB4 follow; AB5 after AB1; AB6 after seven full UTC days on a deployed pinned AB1–AB5 main build |
| Research | [`../research/action-bias.md`](../research/action-bias.md): live store, 30 days to 2026-10-13, findings AB-F1–AB-F7; reference prompts and tools from Codex, OpenCode, Pi, fx |
| Ledger | [`progress/action-bias.md`](./progress/action-bias.md) |
| Linear | [ENG-1037](https://linear.app/retsu-ai/issue/ENG-1037) (program), milestone `Action Bias`; AB1–AB6 are ENG-1038–ENG-1043 |
| ADR | ADR-0059 reserved for AB4 (the prompt and tool set state a run's real authority; supersedes ADR-0054 § 5's read-only-root clause) |
| Crosses | `autonomous-core.md` (progress rule, AC4 loop guard), `tool-layer.md` (built-in contracts, shell nudge), `token-efficiency.md` TE3 and `supervised-delegation.md` D6b (delegation economics). Each slice names what it borrows; § Coordination lists the requests |

## Goal

When a run is asked for a change, it makes the change, verifies it and
reports, without reading the same material repeatedly first. Specifically:

- **Root runs reach their first edit sooner.** Built-in reads before the
  first observed built-in edit: median 9 → ≤ 6, p90 57 → ≤ 30.
  Unknown shell/exec/external mutation paths or ordering leave the whole-run
  first-edit and instruction-coverage gates unqualified.
- **Fewer re-reads.** Same-path re-reads fall from 44 % of root `read_file`
  calls (historical argument-spelling proxy: 1,631 of 3,669) to ≤ 25%.
  Qualification groups successful reads by persisted canonical file identity;
  unresolved historical aliases make the cohort unqualified. Remeasure both
  windows with the same extractor before comparing.
- **Reads go through the built-ins.** Root shell calls matching the pinned lexical read classifier
  (`lexical_read_share`) fall from 38 % of root `shell`
  calls (725 of 1,886, historical program-name proxy) to ≤ 15 %.
  This measures command shape, not execution effects. Inherited PATH/TMPDIR
  and executable identity are absent from historical records, so actual
  nonmutation remains unqualified; progress classification grants no authority.
- **No per-directory instruction walk.** Each run that actually edits below root starts at most one instruction
  names search per permitted discovery root (workspace root plus applicable
  highest ignored subtrees, plus justified target ancestors at 64-component
  boundaries for deeper targets); cursor pages continue that search. The historical
  271 physical probes and the substring proxy (25 starts, seven repeated
  roots across five runs) remain context. The anchored boolean-regex query
  records one discovery start across 108 qualifying roots, no repeated roots.
  Path/page/read coverage remains
  unqualified; the historical raw argument counts are separate context.
  The built-in nested-edit subset is diagnostic. Malformed discovery paths and
  shell/exec/external calls without immutable mutation-path evidence leave
  whole-cohort qualification open, including shell-only edits.
- **The safety net fires.** A root run whose last 64 calls changed nothing
  receives a stall report. Today `cd … && grep` and `sed -n` count as work,
  so this never happened in the window: 0 root stall reports in 30 days
  (children: 21).
- **No regressions.** Edit failure rate, mean wall time of non-spawning
  roots, and completed-run share do not get worse.

The baselines are from the research snapshot. They are observational: the
task mix is not controlled. A paired evaluation is the stronger gate and is
described in AB6.

## Non-goals

- **Ending root runs.** A stuck root is paused by AC4 (ADR-0049 § 8), not
  by this plan. AB5 only changes what the root's stall report asks for.
- **Changing `qq run`'s default approval** (`read-only`). AB4 makes the
  prompt tell the truth about it; changing the default is a product
  decision.
- **Removing or hiding built-ins.** Codex removed its read and grep tools.
  QQ's ablation (T13) has not measured that, and nothing here pre-empts it.
- **Per-model prompt variants.** One prompt; it is measured as one prompt.
- **A delegation limiter or classifier.** Delegation is the largest
  wall-time cost (AB-F4). Its defaults are owned by D6b and TE3; this plan
  hands them the evidence (§ Coordination).
- **Read-range memory or deduplication in the runtime.** If AB2 and AB3 do
  not bring re-reads down, that is a TE5 question, not a new mechanism
  here.

## Principles

1. **Classify reads honestly.** The progress rule decides when a run owes a
   report. It must agree with what the call actually did: a `grep`
   preceded by `cd` is a read.
2. **The prompt states one obligation: finish.** Inspection rules stay only
   where they protect correctness: read before editing by anchor, preserve
   unrelated work, and verify. Rules that only add reads go.
3. **Make the built-in strictly better than the shell form,** and say so
   where the model chooses: the prompt, and the tool's own description.
4. **Describe exactly the authority the run has.** Never offer a tool that
   is certain to be refused, and never say "implement" to a run that cannot.
5. **Measure before and after with the same queries.** The research appendix
   is the runbook.

## Task index

| ID | Goal | Finding | Size | Inputs | Owned paths | Acceptance |
| --- | --- | --- | --- | --- | --- | --- |
| AB1 | A shell call counts as a read when, after a leading `cd DIR` (`&&` or `;`), every segment is a read; `sed -n` (no in-place, no `w`/`e`), `nl`, `cut`, `uniq` (no real output operand) and `git -C DIR <read>` join the read set; known write shapes that pass today stop passing | AB-F1 | S | — | `crates/qq-core/src/approval.rs` (bounded shell/argv read recognition), `runtime/progress.rs` tests, `lib.rs` and `runtime/audit.rs` audit-trigger tests | The § AB1 table as a test, every row; scripted root fixture: 64 × `cd sub && grep -n x f.rs` → a stall report on request 65; `cd sub && cargo test` still resets the count; a short run whose only change is `sed -ni s/a/b/ f.rs` still triggers the heuristic audit; `turn_overhead` within noise |
| AB2 | Prompt v20: an action-biased root contract, an exploration budget, shell-to-built-in mapping including ignored worktrees, instruction files found once per distinct search root instead of a per-directory walk, a header naming only present tools | AB-F2, AB-F3, AB-F5 | S | — | `crates/qq-core/src/runtime/prompt.rs`, `AGENT_PROMPT_VERSION` 19 → 20, `tools/specs.rs` descriptions for `read_file`/`search`/`tree`/`shell`/`exec`, `tools/search.rs` bounded names discovery and depth reporting, the prompt assertions in `lib.rs` (e.g. `observable completion criteria`, `lib.rs:2669`), `docs/design/tools.md` § Tool loop conventions paragraph (`tools.md:55-61`) | Golden tests: root prompt contains the § AB2 bullets verbatim; an analysis-only root task retains an explicit findings-without-edits instruction; a read child keeps its sub-agent section and lacks the "Do the work" bullet; the header omits `search_history`/`read_tool_result` when absent; `plan_compile` within noise; prefix bytes recorded; `search mode=names` and content search with `path` into an ignored directory each have a regression test; exact basename matching excludes NOTAGENTS.md; two distinct ignored roots, ignored-ancestor instructions, and nonignored parent instructions above an ignored subtree are covered without per-directory probes |
| AB3 | `read_file` default window fills the byte budget instead of 200 lines; a read stopped by the line limit carries an admissible continuation or a terminal offset-bound marker; ordinarily `…[qq: N more lines; continue from offset=K]…`; the description says one larger read beats many slices | AB-F3, AB-F7 | S | AB2 (description text) | `crates/qq-core/src/tools/read.rs`, `tools/specs.rs`, `docs/design/tools.md` § read_file | A 3,000-line file with no `limit` returns lines up to the 32 KiB budget with the offset marker; a 300-line file returns whole with no marker; an explicit `limit` stopping early gets the marker; explicit `ranges` are unchanged; a 3,000-line short-line file fitting the byte and rendered-line bounds returns whole; a 4,000-empty-line file gets contiguous offset continuation before generic bounding; a batched fixture (6 parallel default reads of 1,000-line files) records turn-budget cuts before and after, and the slice does not ship if cuts rise without fewer calls; the schema's `"default": 200` (`specs.rs:126`) change is recorded with the tool-schema hash; `tool_dispatch` and `tool_output` within noise |
| AB4 | Direct runs (`qq ask`, `CompiledAgentPlan::run`) and read-only roots declare only tools that can execute, and a read-only root's prompt says it cannot act; ADR-0059 | AB-F6 | M | AB2 | `crates/qq-core/src/lib.rs` (`CompiledAgentPlan::run`, `run_messages`), `catalog.rs` (`StaticFilter` distinguishes strict read-only and unattended direct runs; eligibility honours configuration grants and filters pins/index/selection), `plan.rs` (prefix-cache bound comment), `runtime/prompt.rs` (authority line), `docs/design/{architecture,protocol}.md`, `docs/guide/{headless,cli}.md` (`qq ask` "no tools" is wrong today), ADR-0059 | `qq ask` request declares no `edit_file`/`write_file`/`shell`/`exec`/`ask_user`/`fetch`; a config-granted mutating MCP tool still declares and runs on a direct run; an ungranted hinted-read-only External tool is omitted on direct runs; strict read-only sessions omit all External tools, including hinted/granted tools; a pinned mutating MCP tool is not declared in a read-only run; progressive index/selection does not offer or report an ineligible mutating tool as callable; the read-only root prompt has the authority line and no implement line; a `qq run --approval auto` prompt is byte-identical to AB2's; the `plan.run` callers in `src/main.rs`, `src/runtime.rs` and `qq-core` tests pass; public `Runtime::run_messages` grant changes between stream construction and first poll use the newly compiled snapshot for declarations and execution |
| AB5 | A root's stall report asks for the next action, not a status summary: a new `TurnNotice::RootStallReport` with its own wording; children keep `StallReport` | AB-F1 | S | AB1 | `crates/qq-core/src/lib.rs` (notice constant), `runtime/events.rs` (`TurnNotice`, `from_stored`), `runtime/run_loop.rs` (choose by `StallPolicy`), `sessions/store/schema` (`STORE_SCHEMA_VERSION` 45 → 46, version-only migration; root request), `docs/design/protocol.md` store schema history, guide `troubleshooting.md` and `providers.md` schema mentions (checked by `docs_truth`) | A root fixture gets `root_stall_report` on request 65 and its continuation; a replayed pre-AB5 root run still renders `stall_report` byte-identically; live/replayed read-only roots receive the same authority-neutral notice; a child fixture is unchanged; a schema-45 binary refuses a schema-46 store at open instead of failing per session with `CODEC` |
| AB6.1 | Reproducible shell-read qualification export and classifier recount | AB-F1 | S | AB1 | `xtask/src/{main,action_bias}.rs`, `xtask/Cargo.toml` if SQLite access is needed, existing hidden `approval.rs` support wrapper, research export/recount instructions, ledger | Read-only consistent snapshot with fixed root terminal-by-cutoff cohort; versioned bounded JSONL with schema/window/cutoff/expected count and every shell call ID/run/state/command status; same pinned AB1 classifier binary recounts both windows; malformed/incomplete/duplicate/oversized export tests; accepted and mutating command cases; immutable classifier SHA and binary hash recorded |
| AB6 | Qualification: re-run the research queries after seven full UTC days on the deployed pinned AB1–AB5 main build; if ENG-809 authorizes paid runs, one paired A/B on the T13 fixtures (prompt 19 vs 20) | all | S | AB6.1 accepted; AB1–AB5 shipped | `docs/plans/progress/action-bias.md`, a `progress/g-action-bias.md` gate file | Every § Goal number recorded before and after with the window and schema; any miss becomes a slice or a recorded decision; no claim beyond what the counts show |

Order: AB1 and AB2 in parallel, then AB3, AB4, AB5, and AB6 after its deployed seven-day window.
AB2 and AB4 both edit `prompt.rs`, so AB4 rebases on AB2. AB1 and AB5 both
touch the progress path and land in that order.

## Design notes

### AB1 — what counts as a read

Today `read_only_shell_command` (`approval.rs:493`) is consulted only by
`runtime::is_progress` and the audit trigger (`lib.rs:536`), never by
approval. So widening it changes when reports and audits fire. It does not
change what may run. The slice states this in the function's doc comment
and keeps it true: it does not reuse the approval classifier's `Allow`,
which admits `cargo test`.

The rule:

1. Strip one leading `cd DIR` followed by `&&` or `;`. `DIR` is one word
   without shell metacharacters; quoted or escaped operands are not
   stripped. This is the same lexical test T20 uses for its nudge, without
   T20's containment requirement, because nothing here grants authority.
2. Unquoted redirection or shell metacharacters make the command work.
   Unix single-quoted literals retain regex characters; double-quoted
   expansion still counts as work. On Windows, single quotes and cmd
   expansion/escape characters (`^`, `%`, `!`) count as work; native sort
   counts as work because its output flags differ from GNU sort.
3. A newline, unquoted `$(`, a backtick or `<(` makes the command not a read. The
   splitter today ignores `\n`, so `grep x f` + newline + `cargo fmt` is
   misread as a read. That is fixed here.
4. Every remaining `|`, `;`, `&&` segment must start with a read prefix.
   The current list is kept with these narrowings, which close shapes that
   pass today and write:
   - `find` without `-delete`, `-exec`, `-execdir`, `-ok`, `-okdir`, `-fprint*`, `-fls` and execution-action abbreviations;
   - Git reads use canonical `git [-C DIR] --no-lazy-fetch -c core.fsmonitor=false <read>`.
     Working-tree status, diff and blame remain work because configured
     clean/process filters may execute helpers even with fsmonitor disabled.
     The explicit no-lazy-fetch switch prevents partial-clone object reads
     from fetching and writing packs; older Git without this switch remains work.
     rev-parse and worktree list accept only their closed inspection forms,
     excluding help switches that can launch configured viewers;
   - branch reads require explicit `--sort=refname`, no custom format or
     other sort and listing/filter operands only. Signature atoms in formats,
     explicit sorts or configured default sorting may invoke GPG helpers;
   - history-only `log`/`show` reads start with `--no-ext-diff --no-textconv
     --no-patch --diff-merges=off --no-show-signature --format=oneline`, then
     only supported history/count flags, revisions and literal paths after
     `--`. Unknown switches and patch/stat/raw/remerge/format overrides count
     as work. Explicit disables also prevent configured merge-diff behavior;
   - `cargo metadata`/`cargo tree` are work even with `--locked`/`--frozen`:
     configured compiler wrappers may execute. `cargo --version` and
     `rustc --version` are also work: rustup proxies may install a missing
     workspace-selected toolchain;
   - `rg` reads require leading `--no-config`, then reject `--pre`, hostname helpers (`--hostname-bin`) and compressed
     search (`-z`, short clusters containing `z`, `--search-zip`);
   - `fd` excludes `-x`/`-X` (including short clusters), `--exec` and
     `--exec-batch`, detailed listings (`-l`/`--list-details`, including short clusters); `tree` excludes `-o` (including attached operands) and
     output-option spellings; `file` excludes `-p`/`--preserve-date` (including short clusters and accepted long abbreviations), `-C`/`--compile`, decompression (`-z`/`-Z`, including clusters, and uncompress forms) and accepted long-option abbreviations;
   - JJ commands remain work: even ignoring working-copy snapshots can merge
     concurrent operation heads, and disabling integration still creates
     operations. Diff-producing commands can also invoke configured formatters.
5. A valid trailing semicolon retains the read verdict; incomplete pipelines
   and conjunctions remain work. Any explicit nonempty requested shell/exec
   environment is work: process initialization and helpers may change behavior.
   Malformed or duplicate environment metadata also disqualifies reads without
   altering the command/cwd used for approval.
6. Additions:
   - `sed` print and pattern-space substitution scripts when `-n` is present, no flag contains `i` or is
     `--in-place`, no file-supplied program (`-f`/`--file`, joined or
     abbreviated forms), and every literal script has no `w`, `W` or `e`
     command or substitution `w`/`e` flag. Parse field delimiters/escapes before
     separators, so literal semicolons inside patterns/replacements are data;
   - `nl`, `cut`;
   - `sort` is work: its spills may write inside inherited TMPDIR, even without
     explicit temporary-directory, output or compressor options. No lexical
     command form here proves that inherited context safe;
   - `uniq` with at most one input operand and an optional second operand `-`
     for stdout;
   - `git -C DIR` followed by a read under the same rules, and
     `git worktree list`.

| command | read? |
| --- | --- |
| `cd crates/x && grep -n foo y.rs` | yes |
| `cd crates/x; sed -n 1,80p y.rs` | yes |
| `sed -n 1,80p f.rs \| nl` | yes |
| `git -C .worktrees/a -c core.fsmonitor=false diff --stat` | no (configured filters) |
| `git --no-lazy-fetch -c core.fsmonitor=false branch --list --sort=refname` | yes |
| `git status`, `git diff --stat`, `rg needle f`, `fd -l`, `file -Sz sample.Z` | no |
| `rg --no-config 'foo(bar)' f.rs` | yes on Unix |
| `echo 'x & cargo fmt & echo x'`, `sort input.txt /o output.txt` | no on Windows |
| `cd x && cargo test -p y`, `cargo --version`, `rustc --version`, `sort input` | no (work) |
| `sed -i s/a/b/ f.rs` | no |
| `cd x && grep foo f > out` | no |
| `cd 'a b' && grep x f` | no (not stripped; unchanged from today) |
| `sed -n 's/.*/touch generated/e' input.txt`, `sed -n 's/x/y/w generated.txt' input.txt`, `sort --o=written.txt input.txt` | no |
| `find . -okdir touch written.txt`, `fd --max-depth 1 -x touch written.txt`, `fd -X touch written.txt`, `tree -o written.txt .`, `file -C -m magic` | no |
| `find . -fls written.txt`, `sort -S1K --batch-size=2 --compress-program=./mutating-compressor input.txt` | no |
| `sed -n -f write.sed input.txt`, `sed -n --file=write.sed input.txt` | no |
| `sed -ni s/a/b/ f.rs`, `sed -n --in-place …`, `sed -n '1,5w out' f` | no |
| `sort -S1K -T . huge-input`, `sort --temporary-directory=. huge-input` | no |
| `sort -o f.rs f.rs`, `sort -oout f.rs`, `sort --output=out f.rs`, `sort --output out f.rs`, `uniq in out` | no |
| `git branch --delete topic`, `git branch --move old new`, `git branch --copy old new`, `git branch --force topic` | no |
| `git show --output=written.txt HEAD`, `git log --output=written.txt` | no |
| `find . -name '*.orig' -delete`, `git branch -D topic`, `git diff --output=x` | no (read today; fixed) |
| `grep x f` newline `cargo fmt`, `cat $(cargo fmt)` | no (read today; fixed) |

A false "read" matters in one place besides the stall count. It clears the
audit trigger's `non_read_shell` (`runtime/audit.rs:69-70`), so a short run
whose only change is a misread write is not audited. The rows marked
"fixed" close cases that are wrong today.

Consequence: roots that explore through the shell now receive stall
reports. That is ADR-0054's intended behaviour. It amends no decision, only
how "read-only" is judged, and ADR-0054 § 1's text is clarified in the same
PR (the ADR is Proposed). AC4 later counts novelty over the same
classification and needs no change.

### AB2 — prompt v20

Changes to `Working conventions` (`prompt.rs:260-279`). The final wording
is reviewed in the PR; these bullets are the contract.

- **Replace** "Determine observable completion criteria…", "Implement
  requested changes rather than stopping at analysis…" and "Inspect
  existing state before changing it…" with:
  - "Do the work. Decide what done looks like, read what the change needs,
    make it when a change is requested, verify it, and report. For an
    analysis-only or review request, deliver the findings without changing
    files. A plan, a proposal, or an offer to
    continue is not a result: end your turn when the task is done or a
    concrete blocker remains, and name the blocker."
  - "Explore only as far as your next action needs. Once you can name the
    file and the change, make the change. Search an exact name once; if it
    finds only a definition, act on what you know or say what is missing."
  - "Do not re-read text that is still in your context; re-read only lines
    you have not seen, lines that may have changed, or a read that shows
    as a [pruned: …] stub. Read the region you need in one call, with
    ranges for several windows, rather than paging in slices."
  - "Preserve unrelated work in the files you change."

  The first bullet is dropped for read children, keyed exactly as the
  implement line is today (ADR-0054 § 5). Their exploration bullet asks
  for findings and omits the second demand to make the change. AB4 replaces it for read-only
  roots.
- **Replace** the shell-preference bullet with: "Read with the built-ins:
  read_file instead of cat, head, tail, or sed -n; search instead of grep or
  rg; tree instead of ls or find. They accept a path inside ignored
  directories, such as a worktree under target/ or .worktrees/: pass path
  and include_ignored=true to search or tree rather than cd."
- **Replace** the AGENTS.md walk with: "Instructions from nested AGENTS.md
  files (CLAUDE.md where there is none) apply below their directory, more
  specific first. Before editing below the root, find them once per distinct search root
  with search mode=names regex=true query="(^|/)(AGENTS|CLAUDE)\.md$", and
  read only those on the path to your target. Always search the workspace
  root to cover nonignored ancestors. For a target inside an ignored
  directory, also search its highest ignored ancestor below the workspace
  with path and include_ignored=true, covering instructions within that subtree. Search each distinct
  discovery root once, covering every edit target; read only applicable
  instruction files. Targets deeper than 64 directory components also use
  target-ancestor roots at each 64-component boundary, with path and
  include_ignored=true. Names discovery includes exact instruction basenames despite file-level
  ignores in visible directories; ignored directories still require the
  explicit applicable-root override. It reports safe instruction-file links;
  broken, escaping or nonfile instruction links block editing. partial=depth
  or depth_limited requires these bounded scopes, not a cursor into skipped
  directories. Scan/time stops after completed empty directories also return
  a watermark cursor that skips those fully visited subtrees. Cursor continuations belong to that discovery; do not
  restart it unchanged. The host supplies workspace-root instructions."
- **Shorten** "mode=outline to see a file's shape before reading it" to "mode=outline
  for the shape of a long file". The rest of the T19 bullet is unchanged.
- **Declared tools:** every tool-specific header description and working
  convention is conditional on `specs`, including arbitrary configured
  subsets and no-tools profiles. Capture a read_file-only and a no-tools
  request and assert no absent tool is advertised or instructed. The full
  toolset retains the quoted contract bullets above.
- **Fallout in the same PR:**
  - The `lib.rs` prompt assertion on "observable completion criteria"
    (`lib.rs:2669`) moves to the new wording.
  - `docs/design/tools.md:55-61` lists the conventions and is amended.
- **Tool descriptions:**
  - `read_file`, `search` and `tree` each gain one "Use instead of …" clause.
  - `shell` gains "Not for reading files or searching; use read_file,
    search, tree."
  - Descriptions are part of the tool-schema hash, and the PR records that
    hash change.

Nested names discovery includes safe in-workspace instruction-file symlinks,
without traversing directory links; invalid instruction links return a blocker.
Ignored search/tree calls require both path and include_ignored=true.
The bounded walk reports depth omissions, and targets deeper than 64 directory
components use justified ancestor roots at 64-component intervals. Test safe,
broken, escaping and directory-target links, paginated names and scan-budget
continuations, plus instructions at depths 64, 65, 128 and 130.

Names discovery must bound per-directory enumeration before sorting: at most
100,000 retained entries and 32 MiB of retained name/path bytes, with
cancellation and the scan deadline checked during enumeration and after sorting.
An incomplete unsorted listing returns an explicit budget/cancellation/time
error with no continuation cursor. Test a large single directory, each bound,
mid-enumeration cancellation, deadline expiry and unchanged ordered continuation.

### AB3 — read window

`READ_BOUNDS` (32 KiB) already bounds a read. The 200-line default only
makes small files cost two calls. With no `limit`, the window becomes
an open-ended range (`end: usize::MAX`), and `READ_BOUNDS`
(32 KiB, 4,000 rendered lines) stops it, with the existing
`continue from offset=` marker when that offset is accepted. Once the next
unread line exceeds the 100,000 start-offset bound, use a truthful terminal
marker naming that bound; do not suggest an unusable call. Test a short-line
file beyond that boundary. A read that stops on its line limit
before the end of the file now gets the same marker, with the remaining
line count when the entire file was scanned. Beyond the 4 MiB scan cap,
remaining count is unknown: use an unknown-count marker naming the cap and
an accepted next offset only while unread scanned-prefix rows remain. Once
that prefix is exhausted, emit a terminal scan-cap marker without a cursor.
A single over-cap line is also terminal. Test both bounded-default
and explicit-limit reads of a file beyond the scan cap. Explicit `ranges` are unchanged. The read loop reserves rendered lines
for its header and continuation marker before admitting body rows; generic
head/tail truncation must never replace a contiguous offset continuation.
A 4,000-empty-line no-limit fixture stops contiguously with the offset
marker and reconstructs the complete file across continuation calls.

The risk is the per-turn budget. 96 KiB admits three full 32 KiB reads.
A fourth read in the same batch is cut to at least 4 KiB, and its marker
points at `read_tool_result` (or says "not stored" on a direct run)
rather than at an offset (`tools/output.rs:812-846`). That is more calls,
not fewer. So the slice measures a batched fixture before and after. If
cuts rise without fewer calls, it chooses a smaller default (for example
16 KiB of lines) and records why. Replay is unaffected, because the turn
projection is deterministic (F23).

### AB4 — authority the run actually has

- `CompiledAgentPlan::run`'s doc comment already says "read-only tools
  scoped to the plan's workspace, … Configuration allowlists are the only
  grants." The code passes `RunCapabilities::user` (`read_only: false`) and
  an `Ask` gate that denies everything un-granted.
  - AB4 makes the declared tools match the comment. It declares a tool
    only if it is read-only or a configuration grant covers it. Interactive
    `ask_user` is omitted
    on unattended direct runs because their gate cannot settle questions.
    Feature-enabled captured direct requests and prompts must also lack `fetch`,
    since unattended direct runs cannot approve network hosts. A captured direct
    request and its prompt must lack `ask_user`; prompt
    descriptions and question guidance are conditional on declaration. Interactive session
    runs retain it. Read-only external hints never override this exclusion.
  - Direct-run grants are host grants only (`lib.rs:1407-1412`,
    `shell_prefixes` empty), so built-in edit and shell tools are never
    declared there.
  - Today `StaticFilter.read_only` retains External tools hinted read-only,
    but hints are not approval grants. Strict ReadOnly denies all External
    calls, even hinted or configuration-granted ones; omit them there.
    Unattended Ask retains only configuration-granted External tools,
    regardless of hints. Neither rule changes the gate.
  - The filter distinguishes strict read-only sessions from unattended
    direct runs. Snapshot configuration-granted tool names at plan compile
    time, so eligibility lookup performs no configuration work on requests.
    Use that same eligibility for declarations, pins, index and selection.
    The direct-run gate uses the same immutable compiled snapshot. Host grants
    may change; a plan retains its compiled authority until replaced. Test
    both grant removal and addition after compilation.
  - `StaticFilter` and `PromptPrefixKey` stay `Copy` and the prefix
    cache stays bounded. Its bound comment (`plan.rs:516-519`: "2⁵ … (198)")
    is updated to the new count.
  - `specs_with_pins` (`catalog.rs:611`) bypasses the filter, so a pinned
    mutating tool is declared even in a read-only run. It is made to
    honour the filter. Progressive tool indexes and `select_tools` ranking
    use the same eligibility predicate, so no selector reports a mutating
    tool as pinned/callable when the next request must omit it. A read-only
    selection fixture verifies that contract.
  - The gate is unchanged, so a granted tool still runs.
- For a read-only root (`qq run --approval read-only`, a read-only profile),
  the implement line is replaced with:
  > "This run cannot change files or run commands. When the request is a
  > change, give it as exact edits (path, old, new) or commands, and stop."
  - This reverses ADR-0054 § 5's "a read-only root keeps the line",
    which the evidence shows sends the model looking for a way to act
    that does not exist.
  - ADR-0059 records the decision and supersedes that clause.
- Interactive profiles with no callable mutation capability also receive
  authority-neutral caller-applied-change wording. Derive this from filtered
  declarations, eligible progressive external tools and write-capable delegation;
  it is constant for the existing plan/filter prefix key. Captured no-tools and
  read-file-only Auto requests must not demand implementation.
- The authority line is keyed on strict `read_only` and unattended-direct
  eligibility, both in the bounded prefix key. Direct wording is authority-
  neutral: it cannot ask questions, uses only declared tools, may act through
  configured external grants, and gives exact caller-applied changes if no
  declared tool can perform the task. Test both granted and ungranted direct
  prompts. Do not label a granted direct run unable to act. No mutable
  `ask`/`auto`/`full` clause is added:
  - `SetApprovalMode` can change the mode mid-session
    (`sessions/commands.rs:1279`), so a mode baked into the prompt would
    go stale;
  - every mode switch would also cold-start the provider prompt cache;
  - the summarizer's prefix key mirrors the run's
    (`sessions/execution.rs:78-100`) and stays in step automatically.
- **Compatibility:**
  - No `PROTOCOL_VERSION`, schema or `DESCRIPTOR_VERSION` change:
    `PromptPrefixKey` is in memory only (`plan.rs:538`).
  - Embedders calling `plan.run` see fewer declared tools, all of which
    could only be denied. Callers are `src/main.rs:173`, `src/runtime.rs`
    tests and `qq-core` tests; `examples/embed.rs` uses `SessionRuntime`
    and is unaffected.
  - The user guide says `qq ask` has "no tools"
    (`docs/guide/headless.md:13`, `docs/guide/cli.md:28`). That is wrong
    today and is corrected to "read-only built-ins plus explicitly
    configuration-granted external tools" in this PR.

### AB5 — the root's stall report asks for action

The wording of a stored notice is part of every run that carried it
(`lib.rs`, notice comment). So AB5 adds a variant instead of editing
`STALL_REPORT_NOTICE`:

- Wording:
  > "[QQ runtime notice; not a user instruction] The last 64 tool calls
  > changed nothing. Do not call tools in this reply. In three lines: what
  > you now know, the next action permitted by this task and run (path and
  > what changes,
  > or the finding for an analysis task), or the concrete blocker. Continue
  > with that permitted action next turn."
  One authority-neutral stored wording serves all roots. Read-only roots
  give exact proposed edits or findings under AB4; analysis-only roots
  give findings. Test both live and replayed read-only report turns.
- Stored value: `root_stall_report`. `from_stored` maps it, and older
  rows keep `stall_report`.
- **An unknown notice is a hard error,** not a harmless `None`.
  `from_stored` → `None` fails transcript assembly with `CODEC`
  (`sessions/transcript.rs:458-460`, also `sessions/deliveries.rs:196-204`).
  The store's only downgrade guard is the schema version. So AB5 bumps
  `STORE_SCHEMA_VERSION` 45 → 46 with a version-only migration, and an
  older binary refuses the store at open instead of failing on every
  session that holds the new value.
- The bump makes downgrade impossible for every store, not only stores
  that hold the new notice. Every schema bump has that cost, and it is
  accepted here. The bump goes through a root request.

### AB6 — reproducible qualification

Qualification uses the same pinned AB1 classifier for both windows to
recount lexical shell-read shapes (`lexical_read_share`), not the historical
program-name SQL proxy. Inherited PATH/TMPDIR and executable provenance are
not captured; this receipt cannot qualify actual nonmutation. A fake `grep`
on inherited PATH may write, and `sort -S1K` may spill inside an inherited
TMPDIR even without an explicit `-T`. Execution trust requires separate
authoritative call-time evidence and is outside this lexical metric. Add a
bounded read-only JSONL export/recount tool (AB6.1) before qualification,
using the existing hidden support seam to call `read_only_shell_command`, not
approval `classify`. Export in a consistent read transaction: version, schema,
start/end/cutoff, root terminal-by-cutoff cohort and expected record count;
records carry every shell call ID/run/state/command and malformed/oversized
status. Parse bounded envelopes with the exact shell execution argument type.
Export a required typed `has_requested_env` boolean for valid commands;
nonempty requested environments count as Work. Invalid environment metadata
counts as malformed/nonread, while absent or mistyped qualification metadata
makes the export unqualified. Pin exporter and recount identities, including
parser/environment semantics, to the same executable. Keep every qualifying shell call in the denominator; invalid or
commands rejected by the classifier's own input bound count as nonreads
explicitly, never silently truncated or skipped. An unclassified export-size
failure makes the gate unqualified; size the serialization bound to cover
all classifier-admitted commands, including worst-case JSON escaping.
Stream recount with bounded record bytes/count; reject malformed, incomplete
and duplicate-call exports. Record immutable classifier build SHA and binary
hash; recount both windows with that same binary. Test cohort boundaries and
identity, git -C/cut/sort/uniq/leading cd, every write exclusion, dynamic syntax,
malformed JSON, oversized records and duplicates. The historical SQL proxy
remains context; requery its baseline with this shared classifier.

Use the research appendix's explicit start/end parameters for both windows.
The historical baseline is 30 days ending 2026-10-13 UTC; the candidate is
seven full UTC days beginning at the next UTC midnight after AB1–AB5 are
merged, one pinned runtime build is installed and prior writers are retired.
Record merge SHAs, runtime source SHA and executable hash, startup time and
exclusive-writer continuity (or equivalent per-run runtime provenance), window
boundaries, store schema and prompt versions. Prompt version or merge dates alone
do not establish treatment. Mixed or unverifiable builds leave the candidate
cohort unqualified. Installation and retirement are deployment prerequisites,
not actions authorized by this plan. Fix observation
to end + 24 hours for both windows and do not query before that cutoff.
Completion counts every started root in the denominator and only roots
completed before the cutoff in the numerator; later completion cannot
change its classification. Record roots not terminal by the cutoff. Other
activity metrics use the terminal-by-cutoff cohort. Report counts
and denominators alongside rates; these unequal windows are observational,
not a causal or statistical test. A zero denominator is unqualified.

The supplemental appendix queries define edit failures, completed-run
share and instruction searches per editing root. No-regression tolerance
is zero: failure share must not rise, completed share must not fall, and
non-spawning root mean wall time must not rise. Any miss or inadequate
sample becomes an open slice or an explicit decision; do not call it a pass.
The instruction gate counts distinct search roots per qualifying run and
permits at most one new names-search discovery per root; cursor pages are continuation,
not a repeated discovery. Failed, denied and interrupted matching starts still
count toward repetition; completed pre-edit result/coverage is a separate gate. Ordinary single-root runs still
allow one; multi-root runs must cover every target scope. Record per-run
roots/discovery starts, physical calls and every repeated root; an average cannot hide repeated
probes. Exclude only completed edit_file calls whose dry_run is JSON boolean
true from the actual editing cohort. Failed previews remain executed edit
attempts, and must remain in failure numerator and denominator. The historical global
probe count remains context, with its separate denominator stated.

Paid paired comparison requires ENG-809 authorization and an evaluator
slice before dispatch. Existing `xtask eval compare` rejects differing
prompt versions; it must keep doing so for ordinary comparisons. The
prerequisite slice adds an explicit AB2-treatment mode. Build both arms
from the same accepted AB1–AB5 runtime source; a reviewed counterfactual
patch changes only AB2's prompt treatment and four descriptions, retaining
identical AB3 schemas/behavior and AB4/AB5 authority/notice behavior, including
base prompt version 22. Label
this prompt-19-versus-20 treatment on a fixed runtime, not a comparison to
the historical released prompt-19 runtime. Record the exact expected pairs
for source revision, binary hash, prompt/system hashes, tool-schema hash and
derived catalog identity. The treatment mode validates only those recorded
pairs and preserves every other comparator check; identical T13 fixtures,
model and configuration remain required. Tests reject unrelated pairs,
unlisted identity changes and differing input schemas/runtime behavior.
A pre-stack versus post-stack comparison is explicitly AB1–5 treatment,
not attributable solely to AB2; it requires separate declared identity
pairs and authorization. Until that
slice is accepted, the paired branch is blocked; no paid run or comparator
bypass is authorized by this plan.

## Coordination

| With | Request | Why |
| --- | --- | --- |
| `autonomous-core.md` ledger | AB1 and AB5 touch the AP3b progress path (`progress.rs`, notice set). Run AB1 after AP5's window closes (2026-10-13 17:42 UTC) so AP5's evidence is not mixed | Owner of ADR-0054 |
| `autonomous-core.md` AC4 | Count novelty over AB1's read classification; no change requested | One definition of a read |
| `tool-layer.md` | AB2 and AB3 edit built-in descriptions and `read_file` defaults; T13's arms use the accepted runtime's current identity; historical 19/20 label only the AB2 treatment | Owner of built-in contracts |
| `token-efficiency.md` TE3, `supervised-delegation.md` D6b | Research § Delegation dominates wall time: 145 vs 24 min and 73 % of reads in children. Use as TE3 input; this plan does not change delegation defaults | Owners of delegation economics |
| `progress/root.md` | ADR-0059 reservation; `docs/README.md`, `docs/plans/README.md`, `docs/plans/progress/README.md` rows; at AB5, store schema 45 → 46 (version-only) | Shared files |

## Risks

- **More stall reports on roots that read legitimately.** An analysis-only
  request is mostly reading; it will now get a report at 64 calls. That is
  the existing contract for children and costs one turn. AB5's wording
  allows "the blocker that prevents one". An analysis task can answer with
  its finding.
- **Larger default reads cost tokens.** AB3 measures it. If assembled input
  rises without fewer calls, revert the default and keep only the marker.
- **Prompt changes are model-sensitive.** One prompt is measured as a unit.
  AB6 compares windows; a paired run is the real test and needs ENG-809.
- **Nested-instruction compliance.** Replacing the walk with one search
  could miss a scope the walk found. The AB2 golden test pins the
  semantics; AB6 counts instruction-file reads on the path to edited files.
