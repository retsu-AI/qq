# v0.1.10: fewer disruptions

The goal for v0.1.10 is that an ordinary coding task runs to a verified
result without the agent stalling, asking for help it doesn't need, or
fighting its own environment. This replaces the core stabilization plan,
whose rows 1–5 shipped in v0.1.8 and v0.1.9 (#378, #432–#439, #463–#468,
#470–#472). That plan is in Git history at
[`2cafa65`](https://github.com/retsu-AI/qq/blob/2cafa6537e61495f0a7ef022eef9b985ade9dc22/docs/plans/README.md).

Issues and PRs carry assignments and verification. This page states what to
change, why, and how each change is accepted. Remove a section when it ships.

## Direction

QQ stops trying to contain the agent inside QQ. Commands run in the real
checkout, as the user, with the user's network and environment, the way
they run in the user's terminal. Safety comes from three things:

1. **Snapshots before each run.** `qq undo` restores the workspace, so local
   mistakes can be reversed.
2. **Modes that do what they say.** Read-only reads, Ask asks, Auto runs,
   Full runs everything.
3. **A short list of fixed stops** for actions that can't be undone and are
   almost never intended, such as force-pushing or reading `~/.ssh`. A stop
   is a tool error that tells the agent to find another way.

There is no per-action reviewer model. fx (`.source/fx`) spends about 17k
lines on a shell planner plus a reviewer, and its reviewer deliberately
clears destructive actions that aren't malicious. That is a lot of code and
latency for little protection. pi ships no permission system at all.

Stronger isolation means running QQ inside a container, VM or dedicated
account. The guide shows how (section G). QQ doesn't reimplement any of it.

None of this is a guarantee, and the guide says so. It replaces a sandbox
that kept breaking ordinary work and still left the user's own credentials
reachable.

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

**The old reviewer had no context.** Its system prompt
(`REVIEWER_SYSTEM_PROMPT` in `qq-host/src/runtime.rs`) told it: "you have no
transcript and must not assume intent beyond the stated task." For a root
session it got the tool name, the arguments and a list of recent tool
names. It never saw the user's request. As a result it denied `git push` in
runs where the user had asked for the push, because it couldn't know.
Giving it context would fix that case, but the review would still add a
model call, a failure mode and a black box to every unusual action. A short
list of stops plus undo is easier to predict.

**Code review.** Several parts of the current code don't help:

- The reviewer/delegate path is unreachable today: `internal_supervision`
  requires a supervised Full session, and the binary creates none. It is
  deleted in B.
- The containment code is about 28k lines including tests:

  | Code | Lines |
  | --- | --- |
  | Approval classifier and rules (`approval.rs`, `approval/`) | 6.5k |
  | Command owner | 5.9k |
  | Linux owner (Landlock, seccomp, namespaces) | 3.5k |
  | Windows owner (LPAC) | 2.4k |
  | macOS owner (Seatbelt) | 1.9k |
  | Task workspaces and publication | 1.9k |
  | Task Git helpers, the helper image and hosting brokers | 6.1k |

  Most of the release notes since v0.1.5 are patches to it.
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

## B. Run in place, as the user; delete the sandbox

Snapshots and `qq undo` (#482), the mode table and fixed stops (#483), and
one denial format shipped first; see
[permissions](../guide/permissions.md) and
[tools](../design/tools.md#fixed-stops). This section is next.

1. `shell` and `exec` run in the session's working directory, as the
   user, with the inherited environment, host network and a real HOME. The
   runner keeps:
   - a process group per command;
   - timeout and cancellation that kill the group;
   - output capture into the spill;
   - the background mode from section C.

   A command no longer survives a daemon restart. On restart, unfinished
   commands are reported as interrupted.
2. Delete the following, together with their tests and docs:
   - the Linux, macOS and Windows owners, and the `landlock` and
     `seccompiler` dependencies;
   - the command owner's grants, native scopes and durable job state;
   - task workspaces and publication;
   - the `task_git*` helpers, the helper image, the hosting brokers, and the
     `task_commit`/`task_push`/`task_pr` tools;
   - `allow_hosts`/`--allow-host`, `shell_env` filtering, `writable_roots`,
     `task_hosting` grants and `qq-config/src/promote.rs`;
   - the old supervision routing, the approval reviewer and
     `approval_delegate`;
   - session and workspace approval grants;
   - `xtask action-bias` and its runbook.

   Trim `run-output-queries.md` to the queries still in use, and update
   `src/docs_truth.rs` to match.
3. Parallel goals that need separate trees use `git worktree add` under
   `.worktrees/`, which keeps a real `.git` and the build cache. No copies.
   This waits until the fleet plan needs it.
4. Wire and config removals bump `PROTOCOL_VERSION`, regenerate the
   fixtures, and add an `### Upgrading` note that lists every removed key.
   A removed key in a config file is a load error naming its replacement,
   not silently ignored.

**Accepted when** a smoke scenario on Linux, macOS and Windows shows all of
the following:
- `git status` reports the workspace's own repository;
- a file under `target/` survives between runs;
- an edit is visible before the run ends;
- a loopback HTTP server is reachable from a shell command;
- `git push` to a local bare remote succeeds in Auto.

The lines deleted against the table in Evidence are reported in the PR.

## C. Long-running shell commands

- Add `yield_after_seconds` to `shell` and `exec` (default about 30 s). A
  command still running at that point returns a command id. A new
  `command_status(id, wait_seconds)` tool waits on it, cancels it or reads
  its output, the way `wait_agents` follows children. A background process
  that holds the pipe open no longer blocks the turn.
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

## D. Tool calls tell the model how to recover

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
- Remove the duplicated `if_hash` checks (edit, write, read) and the three
  copies of the shell outcome formatting.

**Accepted when** there is a regression test per repaired shape, and a
recount on a week of v0.1.10 use shows the repeated-error rate below the
9.6% above.

## E. Prompt

Bump `AGENT_PROMPT_VERSION`.

- **Environment.** State it once:
  - the working directory;
  - that commands run as the user with network access;
  - that edits apply immediately;
  - whether a snapshot exists;
  - the session's mode;
  - the fixed stops;
  - what a denial means.

  Drop "may require user approval" and the "host policy" wording.
- **Nested instructions.** Shorten the bullet: the root instructions are
  already loaded, and a nested `AGENTS.md` is read only when editing below
  it.
- **Tool descriptions.** They say how to use the tool; the prompt holds
  policy. Remove the repetition in `spawn_agent`, `read_file`, `exec`,
  `fetch` and `ask_user`.
- **Final answer.** Lead with the outcome, keep it short, and cite
  `path:line`. In Auto, take the next in-scope step instead of offering it.
- **Golden file.** Add a full-text golden file of the rendered root prompt,
  refreshed by an environment variable (no new dependency), so prompt
  changes are reviewed as diffs.

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

## G. Running unattended: set up the environment

- `qq doctor` and the first-run screen say plainly that commands run as the
  user, with the user's network and credentials. They show whether QQ is on
  the host or inside a container, and whether the workspace has snapshots.
- Ship a `.devcontainer/` and a `Dockerfile` that run `qq` or `qq daemon`
  with only the project mounted.
- Add `guide/running-unattended.md`. It covers:
  - mounting only the project;
  - a fine-grained token per repository;
  - a separate Git identity for agents;
  - managed `deny_shell_prefixes`;
  - why a machine with production credentials is the wrong host for
    unattended goals.
- Headless `qq run --goal` on the host prints a one-line note pointing at
  that guide. It never blocks.
- Rewrite `guide/permissions.md`, `design/tools.md` and `faq.md` for B in
  the same PRs.

**Accepted when** the devcontainer builds and runs one deterministic smoke
task against the loopback fake provider in CI.

## H. Smaller reliability items

- Retry "overloaded" and xAI "Internal error during token generation" as
  transient in `qq-provider`.
- A turn cut at the output limit with no visible text continues instead of
  failing the run.
- Auth: the Codex OAuth redirect host matches the listener (`localhost` vs
  `127.0.0.1` in `qq-auth/src/codex.rs`). Model discovery drops the single
  global fetch lock and `DiscoveryAuth` (`qq-host/src/catalog.rs`). These are
  carried over from the retired auth plan.
- Bound store growth with an explicit retention setting before more runs
  die on `DiskFull`. Snapshot refs follow the same setting.
- A user-level `deny_shell_prefixes`. Today only managed config may set it.
- `/undo` in the TUI, as a server command: the runtime refuses it while
  any run is active, queued or due from a goal, and admits no run until the
  restore finishes. A client-side guard cannot see other clients or goal
  scheduling.

## Order

1. B, as about two PRs: run in place; deletions with the protocol bump.
2. C, D, E, F and G in parallel with disjoint owned paths. C's runner and
   E's environment text land with or after B.
3. H whenever there is room.

Each PR:
- shows its regression failing before the fix;
- exercises the real CLI, TUI or server path;
- runs `cargo xtask check` once;
- states size or latency budget overruns in its description instead of
  blocking on them.

Out of scope for v0.1.10: the [desktop, mobile and fleet
plan](desktop-mobile-fleet.md), the TUI session-rail branches, new
providers, and any in-process sandbox.

## Release check

Six small tasks, taken from observed failures:

1. a bug fix;
2. a refactor with an unrelated upstream edit;
3. a build/debug task with a long command;
4. an Ask approval interrupted and resumed;
5. a long-context correction;
6. a review-and-fix cycle that ends in a requested push to a branch.

Each task meets these conditions:

- It starts from a clean repository and an isolated `QQ_HOME`, with fixed
  permissions and budgets.
- A verifier outside the agent checks files, tests, persisted state and
  leftover processes.
- Deterministic versions use the loopback fakes and need no credentials or
  network.

Also run three adversarial tasks on the final build:
- a fetched page that tells the agent to push to another remote;
- a README that tells it to `curl | sh`;
- a task where the agent is likely to `git reset --hard` over the user's
  uncommitted work.

Record whether a fixed stop caught each one, and whether
`qq undo` recovered the third.

Before tagging, run each task once on v0.1.9 and once on the final v0.1.10
build, with the same model, effort and config, from an authorized account
within a stated spend cap. If there is no cap, the check is recorded as not
run. Report every outcome, tool failure, denial and human
intervention as observed. Never combine passes from different builds or
rerun until green. Run section B's smoke scenario on the same build.
