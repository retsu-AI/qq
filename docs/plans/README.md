# v0.1.10: fewer disruptions

The goal for v0.1.10 is that an ordinary coding task runs to a verified
result without the agent stalling, asking for help it doesn't need, or
fighting its own environment. This replaces the core stabilization plan,
whose rows 1–5 shipped in v0.1.8 and v0.1.9 (#378, #432–#439, #463–#468,
#470–#472). That plan is in Git history at
[`2cafa65`](https://github.com/retsu-AI/qq/blob/2cafa6537e61495f0a7ef022eef9b985ade9dc22/docs/plans/README.md).

Issues and PRs carry assignments and verification. This page states what to
change, why, and how each change is accepted. Remove a section when it ships.

## Evidence

**Default Auto, probed on 2026-10-26.** A debug build at `fd15a11` (which
includes #463) ran scripted shell calls through the `verify-qq` fake
provider in a scratch Git repository:

- The shell runs in a private copy under `data/qq/task-workspaces/<run>/`,
  not in the checkout.
- `.git` is not copied. Git searches upward from the copy, so `git status`
  reported the *enclosing* repository's changes; outside one it fails with
  `not a git repository`.
- Ignored directories are not copied, so `target/` is empty on every run and
  every Rust task starts with a cold build.
- There is no network: `cargo build` with a new dependency failed with
  `Could not resolve host: index.crates.io`. No setting enables it.
- HOME is read-only (`Read-only file system`), so tools that write caches or
  config under HOME fail.
- Edits reach the checkout only when the run completes successfully.
- The model is told none of this. The prompt says commands run "under the
  host filesystem, network and environment policy".

The session store had no runs on this build yet, so none of the problems
above show up in the numbers below. Most coding tasks will hit them.

**Session store, 2026-10-03 to 2026-10-23** (schema 63; 490 prompt runs,
19,267 tool calls; mixed builds, all before #463). These are diagnostic
counts, not task success rates:

| Signal | Count |
| --- | --- |
| Runs paused for a human approval (the pre-#463 reviewer escalated) | 127 of 490 (26%), about 10,700 minutes waiting, mostly `git push`/`fetch` and `gh` |
| Tool calls that failed | 13.3%; half were argument-shape errors (`{}` arguments 1,287, mostly before #294; `invalid_ranges` 214; `invalid_edit` with every insert field set 81) |
| Runs repeating the same tool error 3+ times | 47 (9.6%); longest streak 30 calls |
| Shell timeouts | 44, almost all `cargo xtask check`, `xtask perf` or `cargo test` at 600 s |
| Edit failures | stale 57, not found 21, ambiguous 16, not read 12 |
| Completed runs ending in an offer or question | 29 of 394; 24 of them Claude Opus (19% of its runs) |
| Provider failures | 23 runs (4.7%): overloaded, xAI "Internal error during token generation", output cut at the token limit, context overflow |
| Runs killed by `DiskFull` | 5; the store is 1 GB |

**Code review.** Several parts of the current code can't be reached or do no
useful work:

- The reviewer/delegate path is unreachable: `internal_supervision` requires
  a supervised Full session, and the binary creates none. This covers
  `sessions/approvals.rs` review handling, `ModelApprovalReviewer`,
  `approval_delegate`, headless delegate waits, workspace grant promotion
  (`qq-config/src/promote.rs`), `allow_hosts`, and the Ask `a`/`w` keys
  (both resolve once).
- Shell commands have no background mode and no default timeout.
- Capture keeps 128 KiB of head and tail, so the middle of a long log is
  lost; it never reaches the spill.
- A tool argument over 64 KiB fails the whole run
  (`runtime/run_loop.rs`, `MAX_TOOL_ARGUMENT_BYTES`), not just that call.

**Prompt and skills.** The prompt and skill index have these gaps:

- The prompt never states the session's approval mode or environment, and
  Auto prompts still say actions "may require user approval".
- The nested-instruction bullet is about 1.3 KB.
- Several tool descriptions repeat the prompt.
- There is no rule for the shape of the final answer.
- The skill index has no rule for when to load a skill.
- `$ARGUMENTS` is not substituted, the skill's directory is not given, and
  `.agents/skills` is invisible to the model.

## A. Auto runs in place, sandboxed, with network on

This blocks every other section, which all assume it.

1. Auto shell commands run in the real checkout. On Linux (Landlock) and
   macOS (Seatbelt), writes are confined to:
   - the workspace;
   - the temp directory;
   - tool caches (`$CARGO_HOME`, `$XDG_CACHE_HOME`, npm and Go caches);
   - configured `writable_roots`.

   Protected carveouts stay read-only (`.qq/` configuration, pack roots).
   `.git` stays writable. Forbidden rules (force-push, removing `/`) still
   deny.
2. Shell network defaults to the host network. `shell_network: isolated`
   restricts it per project or profile. The existing Linux namespace and
   macOS profile become the restricted option.
3. The private task copy and publication become opt-in
   (`execution: isolated`). Isolated mode keeps them for parallel goals and
   the fleet plan, and offers the `task_commit`/`task_push`/`task_pr`
   brokers only there. Goal completion checks (#448) run in place by
   default.
4. The prompt states the environment once:
   - the working directory;
   - whether the network is available;
   - that edits apply immediately;
   - what a denial means.
5. Windows: granting the LPAC SID access to the real checkout may not be
   acceptable. If it isn't, Windows Auto runs in place without write
   containment and the guide says so plainly. Decide in the first PR.

**Accepted when** a smoke scenario, run on Linux, macOS and Windows, repeats
the probe above. It must show all of the following:

- `git status` reports the workspace's own repository.
- A file under `target/` survives between runs.
- An edit is visible in the checkout before the run ends.
- A loopback server is reachable under the default and unreachable under
  `isolated`.
- A write outside the allowed roots is denied.

Update `guide/permissions.md`, `design/tools.md` and `faq.md` in the same
PRs.

## B. Long-running shell commands

- Add `yield_after_seconds` to `shell` and `exec` (default about 30 s). A
  command still running at that point returns a command id. A new
  `command_status(id, wait_seconds)` tool, built on the existing
  `command_owner` wait and cancel, follows it, the way `wait_agents`
  follows children. A background process that holds the pipe open no
  longer blocks the turn.
- Stream full output into the spill, up to its 8 MiB cap, so
  `read_tool_result` can reach the middle of a build log.
- Set `NO_COLOR=1`, `PAGER=cat`, `GIT_PAGER=cat` and `TERM=dumb` unless the
  caller sets them.
- A timeout result names the limit and suggests a larger timeout or the
  yield mode.
- An argument over 64 KiB becomes a tool error naming the limit.

**Accepted when** each of these is demonstrated by a test:

- A `cargo build`-length scripted command yields, then completes through
  `command_status`.
- `sleep 600 &` returns.
- A 1 MiB log is searchable through `read_tool_result`.
- A 70 KiB `write_file` gets a tool error and the run continues.

## C. Tool calls tell the model how to recover

- Re-measure the top argument errors on a v0.1.9 build with the
  [run output queries](../runbooks/run-output-queries.md) approach before
  changing anything. Fix only the shapes that still occur. Likely fixes:
  - When `ranges` arrives with `offset`/`limit`, prefer `ranges`.
  - Ignore empty `insert_*` fields next to `old`.
  - Accept `edits` sent as a JSON string or a single object.
- Edit matching:
  - preserve CRLF line endings on fuzzy matches (`tools/matching.rs`);
  - normalize typographic quotes and dashes;
  - strip a copied read_file line-number gutter from `old`.
- On the second identical argument error in a run, include a minimal valid
  call in the error.
- One wording for `env_not_allowed`. Remove the duplicated `if_hash` checks
  (edit, write, read) and the three copies of the shell outcome formatting.

**Accepted when** there is a regression test per repaired shape, and a
recount on a week of v0.1.10 use shows the repeated-error rate below the
9.6% above.

## D. Permissions: delete what can't run, make denials actionable

Lands after A.

- Delete the unreachable reviewer and delegate path, workspace grant
  promotion, and `allow_hosts`/`--allow-host`. Protocol and config changes
  need a `PROTOCOL_VERSION` bump with fixtures, and an `### Upgrading` note.
- Ask: `y` approves once, `a` approves the command prefix for the session
  (in memory), `n` denies. Remove `w`.
- One denial format the model can act on:

  ```
  permission_denied: <rule>: <what was refused>; allowed instead: <...>; to allow it: <config key>
  ```

  The prompt says once that `permission_denied` and `forbidden` are final,
  and that `not executed` should be reissued.
- Remove `xtask action-bias` and its runbook. Trim `run-output-queries.md`
  to the queries still in use, and update `src/docs_truth.rs` to match.

**Accepted when** the approval modules lose their dead branches with the
existing Read-only, Ask, Auto and Full dispatch tests still passing. Every
denial string has a test asserting its format.

## E. Prompt

Bump `AGENT_PROMPT_VERSION`.

- State the session's mode only, and drop "may require user approval" from
  Auto.
- Shorten the nested-instruction bullet: the root instructions are already
  loaded, and a nested `AGENTS.md` is read only when editing below it.
- Tool descriptions say how to use the tool; the prompt holds policy. Remove
  the repetition in `spawn_agent`, `read_file`, `exec`, `fetch` and
  `ask_user`.
- Final answer: lead with the outcome, keep it short, cite `path:line`. In
  Auto, take the next in-scope step instead of offering it.
- Add a full-text golden file of the rendered root prompt, refreshed by an
  environment variable (no new dependency), so prompt changes are reviewed
  as diffs.

**Accepted when** the six tasks below show no more offers or questions at
the end of completed runs than the v0.1.9 baseline, with Claude Opus
included.

## F. Skills

- The index tells the model: when the task matches a skill's description,
  `load_skill` it before acting.
- Show each skill's directory, and resolve its relative paths against it.
  Substitute `$ARGUMENTS`. Parse `name:`.
- Disclose `.agents/skills`, removing duplicates of `.qq/skills` by name.
  Add a global root at `~/.config/qq/skills`. Cap the index at about 8 KB.

**Accepted when** there are tests for each rule, and a scripted run loads a
matching skill, resolves a script beside it, and receives the substituted
arguments.

## G. Smaller reliability items

- Retry "overloaded" and xAI "Internal error during token generation" as
  transient in `qq-provider`.
- A turn cut at the output limit with no visible text continues instead of
  failing the run.
- Auth: the Codex OAuth redirect host matches the listener (`localhost` vs
  `127.0.0.1` in `qq-auth/src/codex.rs`). Model discovery drops the single
  global fetch lock and `DiscoveryAuth` (`qq-host/src/catalog.rs`). These are
  carried over from the retired auth plan.
- Bound store growth with an explicit retention setting before more runs
  die on `DiskFull`.

## Order

1. A, as about three PRs: in-place sandbox; network setting; task copy
   opt-in plus prompt facts.
2. B, C, E and F in parallel, with disjoint owned paths.
3. D after A.
4. G whenever there is room.

Each PR:
- shows its regression failing before the fix;
- exercises the real CLI, TUI or server path;
- runs `cargo xtask check` once;
- states size or latency budget overruns in its description instead of
  blocking on them.

Out of scope for v0.1.10: the [desktop, mobile and fleet
plan](desktop-mobile-fleet.md), the TUI session-rail branches, and new
providers.

## Release check

Six small tasks, taken from observed failures:

1. a bug fix;
2. a refactor with an unrelated upstream edit;
3. a build/debug task with a long command;
4. an Ask approval interrupted and resumed;
5. a long-context correction;
6. a review-and-fix cycle.

Each task meets these conditions:

- It starts from a clean repository and an isolated `QQ_HOME`, with fixed
  permissions and budgets.
- A verifier outside the agent checks files, tests, persisted state and
  leftover processes.
- Deterministic versions use the loopback fakes and need no credentials or
  network.

Before tagging, run each task once on v0.1.9 and once on the final v0.1.10
build, with the same model, effort and config, from an authorized account
within a stated spend cap. If there is no cap, the check is recorded as not
run. Report every outcome, tool failure and human intervention as observed.
Never combine passes from different builds or rerun until green. Run the
environment probe from section A on the same build.
