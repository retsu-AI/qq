# Action bias: why QQ runs read instead of finishing

Research snapshot: 2026-10-13, QQ `ce768bb` (prompt version 19, store schema
44). The reference trees under `.source/` were inspected at Codex
`044e74fef3`, OpenCode `1015cb67d0`, Pi `b9ab2a623` and fx `bc968ad6`. This
document records evidence. It proposes work but does not claim any. The plan
that acts on it is [`../plans/action-bias.md`](../plans/action-bias.md).

## Question

The operator reported that the reference harnesses "all produce a lot of
work" while QQ "will continue to read and read and read". QQ also uses
shell `grep` instead of its own `read_file` and `search`. The task was to
find out why, and what would make QQ produce results rather than research
forever.

## Evidence 1 — the live session store

Data: a read-only open (`mode=ro`) of `~/.local/share/qq/sessions.sqlite3`
(schema 44, 281 sessions). The window is the 30 days before 2026-10-13: 412
prompt runs and their 29,956 tool calls. Every count below is over calls
in those runs, so totals agree across tables (12,469 `read_file` calls:
3,669 root, 8,800 child). Counts only; no session content. Queries are in
§ Appendix. "Edit" means a completed `edit_file` or
`write_file`.

### Who reads

| | runs | runs with ≥ 20 calls | of those, no edit | built-in reads (`read_file`/`search`/`tree`) |
| --- | ---: | ---: | ---: | ---: |
| root | 254 | 141 | 40 (28 %) | 5,976 |
| child | 158 | 145 | 138 (95 %) | **15,884 (73 %)** |

All 254 root runs ran under `auto` approval. Children ran `read_only`
(148) or `supervised` (10). Most reading happens in delegated children.
A read child cannot edit by design, so the useful question for children is
whether their reading turns into parent output, not whether they edit.

### Delegation dominates wall time

| root runs | n | mean wall time |
| --- | ---: | ---: |
| without `spawn_agent` | 195 | 23.7 min |
| with `spawn_agent` | 59 | 145.4 min |

After ADR-0054 (prompt version ≥ 16) the gap grew: 23.8 min without spawning
against 208.5 min with it, across 55 and 21 runs. Children
got shorter after AP3b. Their median is 56 calls and 3.1 min, and their p90
is 123 calls. But the parents now wait longer overall: 171 spawns averaged
15.5 min each in the tool call, and 38 of them (22 %) returned an error.
Spawning is therefore the single largest wall-time cost, and it buys reads
that the parent then often re-does (§ Re-reads).

### Root runs read in small slices and re-read

- `read_file` returned a median of **120 lines** per call (p90 470) from
  files with a median length of 1,032 lines. Only 1,899 of 8,876 measurable
  reads covered the whole file.
- 9,580 of 12,469 `read_file` calls passed `ranges`, and 1,564 passed an
  explicit `limit`. The commonest limits were 500, 80, 100, 200 and 400.
  The 200-line default is not the main cause: only 64 of 508 default reads
  stopped at line 200 of a longer file.
- **Re-reads.**
  - Roots read the same path again in the same run 1,631 times out of
    3,669 reads; only 27 of those were byte-identical calls.
  - Children re-read 5,282 times out of 8,800 reads (915 identical).
  - 517 (run, path) pairs were read five or more times, and one path was
    read 105 times in a single run.
- Before its first edit, a root run made a median of **9** built-in reads
  (p90 57) across the 109 runs that edited.

The loop is cheap per call. What costs is that the model does not keep a
file in view: it pages through it again in new windows.

### Shell is used for reading, and the progress rule counts it as work

| root `shell` calls (1,886) | n |
| --- | ---: |
| start with `cd DIR` | 1,262 |
| first program, after an optional leading `cd DIR &&` or `cd DIR;`, is a read (`grep`, `sed`, `cat`, `ls`, `rg`, `head`, `tail`, `find`, `wc`, `nl`, `awk`) | 725 |
| … of which start with `cd DIR` | 623 |
| completed, `cd DIR && <read or git status/diff/log/show>` | 511, in 41 runs |
| directories named by `cd` that are ignored worktrees (`target/stk`, `.worktrees/*`, `target/wt`) | ≥ 852 |

`exec` is rarely used for reading: 49 of 2,200 root `exec` calls.

The worktree pattern explains much of the shell use. The model works in
nested git worktrees under `target/` and `.worktrees/`. Both are gitignored,
and `target` is also one of the built-in generated directories. A probe
against `ce768bb` showed what the built-ins do there:

- `search` and `tree` rooted at the worktree path do find files;
- a workspace-root `search` never descends into it;
- `read_file` with the full relative path works.

The model is never told any of this. Nothing in the prompt or the tool
descriptions says that an explicit `path` into an ignored directory is
searched. So it uses `cd X && grep`, which always works.

**The progress rule misreads these calls.** `runtime::is_progress`
(`crates/qq-core/src/runtime/progress.rs:148`) counts a shell call as
progress when `approval::read_only_shell_command` is false.
`read_only_shell_command` (`approval.rs:493`) splits on `|;&` and requires
every segment to start with an allowlisted read. `cd` is not on the list,
so:

- `cd crates/x && grep -n foo y.rs` is classed as **work**;
- `sed -n 1,80p f.rs` is classed as **work**, because `sed` is not listed;
- the audit trigger (`lib.rs:536`) makes the same mistake.

The reverse error also exists. Some writes pass as reads today, and those
clear the audit trigger:

- `find … -delete`;
- `git branch -D`;
- `git diff --output=`;
- a newline-separated second command (`grep x f` then `cargo fmt`), since
  the splitter ignores `\n`;
- `$(…)` substitution.

A root that explores through `cd … && grep` or `sed -n` therefore resets
its stall count on every call and never receives a stall report. Over the
whole window, root runs received **2 slice-checkpoint reports, 0 stall
reports and 2 continuations**. Children received 21 stall reports and 21
continuations. Read with `cd` treated as
transparent, 11 root runs had a ≥ 64-call stretch without real work.
Six of those 11 ended `failed` and two `cancelled`.

### Edits and approvals do not block roots

- `edit_file`: 975 completed, 264 failed and 7 denied. Most failures are
  on the batch's first edit. In the two days since T19 (#281), all runs
  show 64 completed to 4 failed.
- `shell` and `exec`: 44 denied out of 4,086 root calls. Children: 103 of
  429, almost all under `read_only`.

Approval friction is not why roots fail to finish in practice: the
operator's sessions run `auto`. It does matter for the two entry points in
Evidence 2.

## Evidence 2 — QQ's prompt, tools and loop at `ce768bb`

1. **The prompt has no action bias for roots.** `runtime/prompt.rs:251-279`
   has one sentence on the subject: "Implement requested changes rather
   than stopping at analysis unless the user requested analysis-only work."
   It is surrounded by inspection-first rules:
   - "Determine observable completion criteria … before acting";
   - "Inspect existing state before changing it";
   - "mode=outline to see a file's shape before reading it";
   - "Before changing files below a subdirectory, inspect each directory
     from the workspace root to the target for AGENTS.md; when AGENTS.md is
     absent at one scope, check CLAUDE.md". In the window this produced 178
     `read_file` calls and 93 `search` calls for AGENTS.md or CLAUDE.md, 46
     of them `path_not_found`.

   Only sub-agents are told to "stop as soon as you can answer" and "do not
   re-read text that is still in your context" (`prompt.rs:305-313`).
2. **Shell steering is incomplete.** The prompt prefers search and tree
   "over shell grep, rg, find, and ls" (`prompt.rs:265`). It never names
   `cat`, `sed -n`, `head` or `tail` against `read_file`. The tool
   descriptions (`tools/specs.rs`) say what each tool does but not when to
   use another one. The `hint:` suffix (`tools/shell.rs:225`) fired on
   only 156 shell or exec results in the window. It cannot see past
   `cd DIR; …`, and it is appended after the command already ran.
3. **The authority described can differ from the authority granted.**
   - `qq ask` (`src/main.rs:173`) runs `CompiledAgentPlan::run`
     (`crates/qq-core/src/lib.rs:1650`). Its `StaticPolicyGate { mode: Ask }`
     turns every `RequireApproval` into "Tool approval is unavailable for
     this run; the call was denied."
   - The model still receives `edit_file`, `write_file`, `shell` and `exec`
     (`RunCapabilities::user` sets `read_only: false`).
   - The prompt still says to implement changes. On this path, every edit
     and every non-granted command fails.
   - `qq run` defaults to `--approval read-only` (`src/cli.rs:194`). The
     schemas are filtered (`catalog.rs:585`), but the prompt header text
     (`prompt.rs:254-256`) still describes `edit_file` and `shell`, and
     still says to implement.
4. **The prompt describes tools the run does not have.** The header names
   `search_history` and `read_tool_result` whether or not they are present.
   On a direct run there is no spill store, so truncation markers say "not
   stored" while the prompt tells the model to call `read_tool_result`.
5. **There is no repeat guard yet.** AC4 (ADR-0049 § 8) is planned with
   G0 and not built.

## Evidence 3 — what the reference harnesses do

| Mechanism | Codex | OpenCode | Pi | fx |
| --- | --- | --- | --- | --- |
| Explicit action bias | "bias towards action and carry the user's intended task to completion"; "keep going until the query is completely resolved" (`models.json`, `default.md:125`) | gpt/kimi prompts: "Do not settle for a partial … solution"; "do not just describe" | none (minimal prompt, 4 tools) | "Persist until the task is handled, a concrete blocker is reached, or the user interrupts" (`system_prompt.md:19`) |
| Exploration budget | "Do not waste tokens by re-reading files after calling `apply_patch`" (`default.md:143`) | "do not search again in a loop" (`trinity.txt:86`) | — | "After finding a definition, search its exact name once; if no distinct caller exists, report … the next useful step" (`system_prompt.md:18`) |
| Shell versus built-ins | no read or search tools; removed in #15773 and #15775, so `rg` and `sed -n` run through the shell | "Prefer dedicated tools over shell commands" (`system-prompt.ts:13`) | read: "Use read to examine files instead of cat or sed" (`read.ts:22`) | every description has "When NOT to use: … shell grep / bypass dedicated search tools" (`tools.zig:40-42`), and a shell `cat`/`rg` gets "safer: use read_file" |
| Read window and continuation | 10k-token middle truncation | 2,000 lines / 50 KiB; "Continue reading with offset: N"; "Prefer one larger read over many small slices" | 2,000 lines / 50 KiB; "Use offset=K to continue" | 400 lines (cap 2,000); "use start_line/line_count to read more" |
| Repeat guard | — | legacy: 3 identical calls → permission ask | — | an identical call that already failed → "Do not retry it unchanged", then the loop stops |
| Delegation default | prompt-gated | "Do not spawn subagents unless the user … explicitly ask[s]" (`gpt.txt:31`) | none | prompt-gated |

None of them pre-checks every directory for instruction files on the
model's behalf. OpenCode loads a nearby `AGENTS.md` as a side effect of
`read`, and Codex says root AGENTS.md files "don't need to be re-read".

## Findings

| # | Finding | Evidence | Weight |
| --- | --- | --- | --- |
| AB-F1 | Root stall reports never fired in the window, because `cd … &&` and `sed -n` reads count as work | § Shell is used for reading; 0 root stall reports in 30 days | High: the existing ADR-0054 safety net is bypassed for the most common exploration shape |
| AB-F2 | The root prompt has no action bias and several inspection-first rules, including a per-directory instruction walk | § Evidence 2.1; 271 instruction-file probes | High; cheap to change |
| AB-F3 | Re-reads: the model pages through the same file in new windows | 1,631 root and 5,282 child same-path re-reads | Medium–high |
| AB-F4 | Delegation is the largest wall-time cost, and its evidence is re-read by parents | 145 vs 24 min; 73 % of reads are in children | High, but owned by D6b and TE3 evidence |
| AB-F5 | The built-ins' reach into ignored worktrees is undocumented, so the model shells out | ≥ 852 `cd <worktree>` calls | Medium |
| AB-F6 | Direct and read-only runs describe authority they do not have | § Evidence 2.3–2.4 | Low in the operator's data, high for new users and embedders |
| AB-F7 | Read continuation is implicit when a line limit stops the read | 64 of 508 default reads | Low |

## Appendix — queries

Shell setup as in [`../runbooks/run-output-queries.md`](../runbooks/run-output-queries.md)
§ Setup, with `end="2026-10-13"`. `RR` restricts to prompt runs in the
window:

```sh
RR="SELECT r.id, s.parent_id IS NOT NULL child, r.status FROM runs r JOIN sessions s ON s.id = r.session_id
    WHERE r.kind = 'prompt' AND r.started_at_ms >= strftime('%s','@END@','-30 days') * 1000
      AND r.started_at_ms < strftime('%s','@END@') * 1000"
```

```sh
# Who reads: runs, runs with >= 20 calls, of those with no edit
q "WITH rr AS ($RR), x AS (SELECT rr.id, rr.child,
     (SELECT count(*) FROM tool_calls t WHERE t.run_id = rr.id) n,
     (SELECT count(*) FROM tool_calls t WHERE t.run_id = rr.id AND t.name IN ('edit_file','write_file') AND t.state = 'completed') e FROM rr)
   SELECT CASE child WHEN 1 THEN 'child' ELSE 'root' END, count(*), sum(n >= 20), sum(n >= 20 AND e = 0) FROM x GROUP BY 1;"

# Built-in reads by kind
q "WITH rr AS ($RR) SELECT CASE rr.child WHEN 1 THEN 'child' ELSE 'root' END, count(*)
   FROM tool_calls t JOIN rr ON rr.id = t.run_id WHERE t.name IN ('read_file','search','tree') GROUP BY 1;"

# Reads before the first edit (root): runs, median, p90
q "WITH rr AS ($RR), t AS (SELECT t.run_id, t.name, t.state, t.turn_ordinal * 100000 + t.call_ordinal k
     FROM tool_calls t JOIN rr ON rr.id = t.run_id WHERE rr.child = 0),
   f AS (SELECT run_id, min(k) fe FROM t WHERE name IN ('edit_file','write_file') AND state = 'completed' GROUP BY 1),
   c AS (SELECT f.run_id, (SELECT count(*) FROM t WHERE t.run_id = f.run_id AND t.k < f.fe AND t.name IN ('read_file','search','tree')) n FROM f)
   SELECT count(*), (SELECT n FROM c ORDER BY n LIMIT 1 OFFSET (SELECT count(*) / 2 FROM c)),
          (SELECT n FROM c ORDER BY n LIMIT 1 OFFSET (SELECT count(*) * 9 / 10 FROM c)) FROM c;"

# Root shell reading: calls, cd-prefixed, read program after an optional leading `cd DIR &&` / `cd DIR;`, of those cd-prefixed
q "WITH rr AS ($RR), c AS (SELECT json_extract(t.arguments_json, '\$.command') cmd FROM tool_calls t JOIN rr ON rr.id = t.run_id WHERE rr.child = 0 AND t.name = 'shell'),
   d AS (SELECT cmd GLOB 'cd *' cd, CASE WHEN cmd GLOB 'cd *&& *' THEN ltrim(substr(cmd, instr(cmd, '&&') + 2))
                                         WHEN cmd GLOB 'cd *; *' THEN ltrim(substr(cmd, instr(cmd, ';') + 1)) ELSE cmd END e FROM c),
   w AS (SELECT cd, substr(e, 1, instr(e || ' ', ' ') - 1) p FROM d),
   v AS (SELECT cd, p IN ('grep','egrep','rg','sed','cat','head','tail','ls','find','fd','nl','wc','awk') rd FROM w)
   SELECT count(*), sum(cd), sum(rd), sum(cd AND rd) FROM v;"

# Same-path re-reads: reads, re-reads, byte-identical re-reads
q "WITH rr AS ($RR), x AS (SELECT rr.child, t.run_id, json_extract(t.arguments_json, '\$.path') p, count(*) c, count(DISTINCT t.arguments_json) d
     FROM tool_calls t JOIN rr ON rr.id = t.run_id WHERE t.name = 'read_file' GROUP BY 1, 2, 3)
   SELECT CASE child WHEN 1 THEN 'child' ELSE 'root' END, sum(c), sum(c - 1), sum(c - d) FROM x GROUP BY 1;"

# Wall time of root runs with and without spawn_agent
q "WITH rr AS ($RR) SELECT (SELECT count(*) FROM tool_calls t WHERE t.run_id = r.id AND t.name = 'spawn_agent') > 0, count(*),
          round(avg((r.finished_at_ms - r.started_at_ms) / 60000.0), 1)
   FROM runs r JOIN rr ON rr.id = r.id WHERE rr.child = 0 AND r.finished_at_ms IS NOT NULL GROUP BY 1;"

# Report turns by kind
q "WITH rr AS ($RR) SELECT CASE rr.child WHEN 1 THEN 'child' ELSE 'root' END, m.notice, count(*)
   FROM model_turns m JOIN rr ON rr.id = m.run_id WHERE m.notice IS NOT NULL GROUP BY 1, 2;"

# Instruction-file probes
q "SELECT t.name, t.state, count(*) FROM tool_calls t
   WHERE t.requested_at_ms >= strftime('%s','@END@','-30 days') * 1000 AND t.requested_at_ms < strftime('%s','@END@') * 1000
     AND ((t.name = 'read_file' AND (json_extract(t.arguments_json, '\$.path') LIKE '%AGENTS.md' OR json_extract(t.arguments_json, '\$.path') LIKE '%CLAUDE.md'))
       OR (t.name = 'search' AND (json_extract(t.arguments_json, '\$.query') LIKE '%AGENTS.md%' OR json_extract(t.arguments_json, '\$.query') LIKE '%CLAUDE.md%')))
   GROUP BY 1, 2;"
```


### Supplemental qualification queries (AB6)

These metrics were missing from the original snapshot. Qualification fixes
observation to `end + 24 hours` for both windows; wait until `ready=1`.
`RR_STARTS` is every started run, while `RR` is only runs terminal before
that cutoff. Later completion never becomes completed-by-cutoff. Use explicit UTC
`start` and `end` dates, not a rolling relative date. The historical window
is `[2026-09-13, 2026-10-13)`; the candidate window is seven full days after
all AB1–AB4 merge commits. Keep the store open read-only and record its
schema. The following redefines `RR` for **all** queries above as well.

```sh
start="2026-09-13"
end="2026-10-13"
# Qualification is only valid after this fixed observation cutoff.
q "SELECT datetime('$end', '+1 day') cutoff_utc, strftime('%s','now') >= strftime('%s','$end','+1 day') ready;"
RR_STARTS="SELECT r.id, s.parent_id IS NOT NULL child, r.status FROM runs r JOIN sessions s ON s.id = r.session_id
    WHERE r.kind = 'prompt' AND r.started_at_ms >= strftime('%s','$start') * 1000
      AND r.started_at_ms < strftime('%s','$end') * 1000"
RR="$RR_STARTS AND r.finished_at_ms < strftime('%s','$end','+1 day') * 1000"

# Qualification reads before first actual edit (root): runs, median, p90.
# The historical query above included previews; use this corrected query
# for both baseline and candidate qualification.
q "WITH rr AS ($RR), t AS (SELECT t.run_id, t.name, t.state, t.arguments_json,
     t.turn_ordinal * 100000 + t.call_ordinal k
     FROM tool_calls t JOIN rr ON rr.id = t.run_id WHERE rr.child = 0),
   f AS (SELECT run_id, min(k) fe FROM t WHERE name IN ('edit_file','write_file')
     AND state = 'completed' AND coalesce(json_extract(arguments_json, '\$.dry_run'), 0) = 0 GROUP BY 1),
   c AS (SELECT f.run_id, (SELECT count(*) FROM t WHERE t.run_id = f.run_id AND t.k < f.fe
     AND t.name IN ('read_file','search','tree')) n FROM f)
   SELECT count(*), (SELECT n FROM c ORDER BY n LIMIT 1 OFFSET (SELECT count(*) / 2 FROM c)),
          (SELECT n FROM c ORDER BY n LIMIT 1 OFFSET (SELECT count(*) * 9 / 10 FROM c)) FROM c;"

# Root edit calls: failed, executed terminal calls, failure share.
# Denied/interrupted calls are reported separately and excluded from this
# execution-failure denominator. Count both built-in edit forms.
q "WITH rr AS ($RR) SELECT sum(t.state = 'failed'),
     sum(t.state IN ('completed','failed')),
     1.0 * sum(t.state = 'failed') / nullif(sum(t.state IN ('completed','failed')), 0),
     sum(t.state = 'denied'), sum(t.state = 'interrupted')
   FROM tool_calls t JOIN rr ON rr.id = t.run_id
   WHERE rr.child = 0 AND t.name IN ('edit_file','write_file')
     AND coalesce(json_extract(t.arguments_json, '\$.dry_run'), 0) = 0;"

# Root completion by the fixed cutoff: completed, all started roots,
# completed share, not terminal by cutoff (including later completions).
q "WITH rr AS ($RR_STARTS) SELECT sum(rr.status = 'completed' AND r.finished_at_ms < strftime('%s','$end','+1 day') * 1000), count(*),
     1.0 * sum(rr.status = 'completed' AND r.finished_at_ms < strftime('%s','$end','+1 day') * 1000) / nullif(count(*), 0),
     sum(r.finished_at_ms IS NULL OR r.finished_at_ms >= strftime('%s','$end','+1 day') * 1000)
   FROM rr JOIN runs r ON r.id = rr.id WHERE rr.child = 0;"

# Instruction names searches per root that successfully edits below root.
# edit_file uses edits[].path; write_file uses path. Paths are normalized
# workspace-relative paths; absolute/./ forms are exposed separately below.
# Count attempts, including failed/denied calls. Match both legacy separate
# names and the new grouped regex `(AGENTS|CLAUDE)\.md$`.
q "WITH rr AS ($RR), paths AS (
     SELECT t.run_id, json_extract(e.value, '\$.path') path
     FROM tool_calls t JOIN rr ON rr.id = t.run_id JOIN json_each(t.arguments_json, '\$.edits') e
     WHERE rr.child = 0 AND t.name = 'edit_file' AND t.state = 'completed'
       AND coalesce(json_extract(t.arguments_json, '\$.dry_run'), 0) = 0
     UNION ALL
     SELECT t.run_id, json_extract(t.arguments_json, '\$.path')
     FROM tool_calls t JOIN rr ON rr.id = t.run_id
     WHERE rr.child = 0 AND t.name = 'write_file' AND t.state = 'completed'),
   qualifying AS (SELECT DISTINCT run_id FROM paths
     WHERE instr(path, '/') > 0 AND path NOT GLOB './*' AND path NOT GLOB '/*'),
   counts AS (SELECT q.run_id, (SELECT count(*) FROM tool_calls t
     WHERE t.run_id = q.run_id AND t.name = 'search'
       AND json_extract(t.arguments_json, '\$.mode') = 'names'
       AND (instr(json_extract(t.arguments_json, '\$.query'), 'AGENTS') > 0
         OR instr(json_extract(t.arguments_json, '\$.query'), 'CLAUDE') > 0)) n FROM qualifying q)
   SELECT count(*), coalesce(sum(n),0), coalesce(max(n),0), coalesce(sum(n > 1),0),
     (SELECT count(*) FROM paths WHERE path GLOB './*' OR path GLOB '/*') FROM counts;"
```

Corrected qualification baseline (schema 45, fixed window and cutoff above):
109 root runs with an actual edit, median **9** reads and p90 **57**.
The preview exclusion preserves the original 9/57 thresholds. A deterministic
fixture with two reads, a dry-run preview, forty reads, then a real edit
returns 42 reads; a preview-only run is excluded. Evidence is recorded in
`target/qq-perf/ab-plan-2026-10-14/first-actual-edit.json`.

Instruction columns: qualifying roots, names-search attempts, maximum per
run, runs exceeding one, paths requiring normalization. This is the raw
per-run diagnostic; runs over one require the scope query below, because
one search per distinct discovery root is allowed. Dry-run previews are
excluded from the editing cohort. If the last
column is nonzero, normalize those paths against each run's persisted
workspace before qualification; this query alone is insufficient. Zero
searches may mean instructions were already supplied; this count cannot
prove instruction compliance or that the search covered the edit path.
Inspect that coverage separately without publishing session content.

All no-regression tolerances are zero (observational): failed edit share
and non-spawning root mean wall time do not rise, completed share does not
fall. Record numerator, denominator and unfinished runs. A missing or zero
denominator is unqualified; any miss remains open pending a slice or an
explicit decision. Unequal windows and uncontrolled task mix do not prove
causality. The old global probe count (271) is a separate context metric,
not this per-root names-search gate.

Supplemental baseline re-run on 2026-10-14, same historical window,
read-only schema 44: root edit calls 211 failed / 1,230 executed terminal
calls (17.1545%), 0 denied, 0 interrupted; roots 190 completed / 254
started (74.8031%), 0 unfinished; instruction searches 44 attempts across
108 roots editing below root, maximum 13 in one root, 9 roots exceeding
one, 0 paths requiring normalization. Root-only edit counts differ from
the original all-run `edit_file` figures because this metric includes
`write_file` and excludes children. These are supplemental measurements,
not rewritten original evidence.


For the scope exception, use the same terminal editing cohort and count
instruction-name searches by `(run_id, normalized path)` (absent path is
`.`). Record each run's distinct roots and count of roots searched more
than once. A multi-root exception is valid only when the searches cover
every edited target and its instruction ancestors. Coverage is reviewed
separately; fewer searches cannot prove compliance. Keep the original
per-run-over-one diagnostic alongside scope counts rather than relabelling
all such runs as passing.

```sh
# Scope counts: runs, distinct roots, searches, repeated roots, affected runs, maximum roots in one run.
q "WITH rr AS ($RR), paths AS (
     SELECT t.run_id, json_extract(e.value, '\$.path') path
     FROM tool_calls t JOIN rr ON rr.id = t.run_id JOIN json_each(t.arguments_json, '\$.edits') e
     WHERE rr.child = 0 AND t.name = 'edit_file' AND t.state = 'completed'
       AND coalesce(json_extract(t.arguments_json, '\$.dry_run'), 0) = 0
     UNION ALL
     SELECT t.run_id, json_extract(t.arguments_json, '\$.path')
     FROM tool_calls t JOIN rr ON rr.id = t.run_id
     WHERE rr.child = 0 AND t.name = 'write_file' AND t.state = 'completed'),
   qualifying AS (SELECT DISTINCT run_id FROM paths
     WHERE instr(path, '/') > 0 AND path NOT GLOB './*' AND path NOT GLOB '/*'),
   discovery AS (SELECT t.run_id, coalesce(nullif(rtrim(json_extract(t.arguments_json, '\$.path'), '/'), ''), '.') path, count(*) n
     FROM tool_calls t JOIN qualifying q ON q.run_id = t.run_id
     WHERE t.name = 'search' AND json_extract(t.arguments_json, '\$.mode') = 'names'
       AND coalesce(json_extract(t.arguments_json, '\$.cursor'), '') = ''
       AND (instr(json_extract(t.arguments_json, '\$.query'), 'AGENTS') > 0
         OR instr(json_extract(t.arguments_json, '\$.query'), 'CLAUDE') > 0)
     GROUP BY 1, 2),
   per_run AS (SELECT q.run_id, count(d.path) roots, coalesce(sum(d.n),0) searches,
     coalesce(sum(d.n > 1),0) repeated_roots FROM qualifying q LEFT JOIN discovery d ON d.run_id = q.run_id GROUP BY 1)
   SELECT count(*), sum(roots), sum(searches), sum(repeated_roots), sum(repeated_roots > 0), max(roots) FROM per_run;"
```

Paths with `./`, doubled separators or symlink aliases require canonical
normalization against the run workspace before qualification. The query
normalizes absent paths and trailing slashes only; record unresolved
normalization as unqualified, never as a distinct-root allowance.

Workspace-root discovery covers nonignored ancestors for every target.
Ignored targets also require discovery from their highest ignored ancestor
below the workspace. These are the permitted discovery roots. Arbitrary
per-directory paths do not qualify as distinct-root exceptions. Cursor
pages continue one discovery; the scope query counts only new starts,
while the raw diagnostic above retains physical call counts.
