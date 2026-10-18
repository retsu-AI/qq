# Desktop App, Projects, and Parallel Autonomous Work

Status: proposed 2026-10-18 ([ENG-1073](https://linear.app/retsu-ai/issue/ENG-1073)).
Ledger: [`progress/desktop.md`](./progress/desktop.md).
Decisions: [ADR-0017](../adr/0017-client-ui-stack.md) (client UI stack),
[ADR-0018](../adr/0018-apps-workspace-and-integration-branch.md) (`apps/`
workspace and the `feat/desktop` integration branch),
[ADR-0064](../adr/0064-managed-worktree-workspaces.md) (managed worktrees and
parallel writers); ADR-0065 reserved for the coordinator (K8).
Supersedes the U, D, and M phases of
[`multi-surface-clients.md`](./multi-surface-clients.md); that plan keeps W3
and S2–S6, which this plan consumes as inputs.

This plan delivers an open-source QQ desktop app that manages many projects
on many machines and runs many agents on them at once, with QQ as the only
engine that talks to models. It also names every change the engine needs, so
that the CLI, TUI, desktop, and later mobile app expose the same capabilities
over one protocol.

## Outcome

A developer opens the app and sees, in under a second, the answer to one
question: **what needs me?** Everything else stays quiet until asked for.

From there they can:

1. **Keep a list of projects.** A project is a repository (or a set of
   folders) on a machine they own: this laptop, a workstation, a home server.
   Projects from different machines sit in one list.
2. **Start work in seconds.** Type a task, pick a project, choose *this
   checkout* or *a new worktree*, optionally fan the same task out to several
   models or several attempts, and send. The next draft opens immediately.
3. **Run many agents in parallel without collisions.** Every writing agent
   gets its own managed git worktree. Agents never race on one checkout.
4. **Hand off larger bodies of work.** A project can have a *coordinator*:
   a long-lived agent the user talks to, which plans the work, starts worker
   agents in their own worktrees, reviews what they return, and brings the
   integrated result back for approval. Work keeps going when the laptop
   lid closes, because it runs on the server that owns the project.
5. **Stay in control without babysitting.** Agents run under explicit
   approval policy and budgets. They come back only when they need a
   decision, hit a blocker, or finish. Approvals, questions, failures, and
   finished-but-unread work collect in one Inbox across every machine.
6. **Review and land the result.** Each task shows what it changed, what it
   ran, what it cost, and whether its check passed. The user reads the diff,
   reverts or forks from any turn, and commits, pushes, or opens a pull
   request, each behind an explicit approval.
7. **Pick it up anywhere.** The same sessions are visible and steerable from
   the TUI, the CLI, a second desktop, and later a phone.

## Principles

These decide disputes in review. Each one is checked by a named gate or by
the review checklist in § Quality.

1. **The engine owns the work; clients are views.** Projects, worktrees,
   tasks, coordinators, goals, budgets, approvals, and change tracking live
   in `qq-core` and the protocol, never in the app. A feature that exists
   only in the desktop app is a bug unless it is presentation.
2. **One reducer, one protocol, many surfaces.** Session state is reduced
   once, in `qq-client`, and every surface renders it. TypeScript never
   re-implements protocol or state logic (ADR-0017).
3. **Parity by default.** Every engine capability ships with a CLI or TUI
   entry point in the same slice or the next one. The desktop may present it
   better; it may not be the only way to reach it (§ Surface parity).
4. **Attention first, calm otherwise.** The UI shows what needs the user,
   then what is moving, then everything else on request. A running agent is
   one line of live status, not a scrolling log.
5. **Instant.** Input is acknowledged at once, panels open without
   animation, switching sessions never waits on the network when the data is
   already local, and nothing pretends to work: no spinner without a
   durable reason behind it.
6. **Autonomy through explicit policy.** More autonomy comes from approval
   modes, budgets, checks, and isolation the user chose, never from hidden
   behavior. Anything destructive or externally visible is approved.
7. **Bounded everywhere.** Concurrent agents, worktrees, queues, view
   patches, transcript windows, and notifications all have stated limits and
   backpressure, as in the rest of QQ.

## Concepts

The vocabulary below is shared by the engine, the protocol, every client,
and the guide. New code uses these words and no synonyms.

```text
Server       one `qq serve` and its store: identity, connection state, version
 └ Project   a named unit of work on that server: one or more root folders,
   │         default model/profile/approval, worktree setup, coordinator
   ├ Workspace  a checkout the agents work in: the project's main checkout
   │   │        or a managed worktree (branch, base, dirty, ahead/behind)
   │   └ Session   one agent conversation (exists today), with runs, goal,
   │       │       budget, approval mode, accounting
   │       └ Child session  a delegated sub-agent (exists today)
   └ Task    a unit of delegated work: brief, assigned session, workspace,
             state, result (changes, check, summary); created by the user
             or by the project's coordinator
```

- **Server** exists (`ServerId`, S1).
- **Workspace** exists as a resolved path (`WorkspaceId`); this plan adds
  ownership by a project and the managed-worktree kind (K4, ADR-0064).
- **Session**, **child session**, **goal**, **budget**, **approval mode**,
  and **approval delegate** exist and keep their contracts.
- **Project** and **Task** are new engine objects (K1, K8). A task is not a
  new execution path: it is a durable record that links a brief to a
  session in a workspace, so a board, a coordinator, and `qq task list` all
  read the same thing.
- **Coordinator** is a session in a project with orchestration tools and no
  file-writing tools (K8). It is how "hand off a body of work" is built
  without a second agent runtime.
- **Attention** is a derived, ranked status computed in `qq-client` from
  summaries (C1), so every surface sorts and badges identically.

## Architecture

```text
  apps/ui        React + TypeScript renderer: one bundle, every surface
     │  typed view patches ▲ │ typed intents       (generated TS types)
     │                     │ ▼
  apps/desktop   Tauri v2 shell ── qq-client view core (native Rust)
  apps/web       later: same UI ── qq-client view core (WASM, Web Worker)
  apps/mobile    later: Tauri mobile shell, same UI and core
                         │ ServerSet: N servers, one bounded loop each
                         │ HTTP commands + cursor-addressed SSE
         ┌───────────────┼────────────────┐
     qq serve        qq serve         qq serve      (laptop sidecar,
     store, runtime, worktrees, coordinator, tasks   workstation, home)
```

- **Engine.** Unchanged in shape: `SessionRuntime` behind `qq serve`
  (ADR-0001). All new orchestration is `qq-core` state, persisted before
  it is published (ADR-0003), reachable through `qq-protocol` commands and
  events.
- **Client core.** `qq-client` gains `servers` (W3: many servers, bounded
  per-server loops, aggregated overview) and `view` (C2: stable view models
  and sequenced patches). It runs natively inside the desktop process, and
  compiles to WASM for the browser and mobile (it already builds for
  `wasm32`).
- **UI.** `apps/ui` renders view models and sends intents. It owns layout,
  focus, keyboard, and local preferences only. Its TypeScript types are
  generated from the Rust view and intent types, never written by hand.
- **Shell.** `apps/desktop` is a Tauri v2 app: window, tray, notifications,
  deep links (`qq://`), keychain, updater, and a supervised bundled `qq`
  sidecar for the local machine. It attaches to a running local server
  instead when one exists, so terminal and desktop share sessions.
- **Repository.** `apps/` is its own Cargo and pnpm workspace; root gates
  never build Tauri or Node (ADR-0018). App work integrates on the
  long-lived `feat/desktop` branch; engine slices merge to `main` and are
  merged forward into `feat/desktop`.

### Why the view layer sends patches

A client that re-serializes a whole project per frame does work proportional
to transcript length on every streamed token, multiplied by the number of
visible agents. The view core instead keeps a stable model per subscription
and emits sequenced patches on three channels:

| Channel | Contents | Subscribed |
| --- | --- | --- |
| `overview` | servers, projects, workspaces, session rows, tasks, attention counts | always |
| `session(id)` | transcript items keyed by item id: `upsert`, `append_text`, `remove`, window bounds | while a view shows it |
| `panel(id, kind)` | diff, changed files, plan/goal, tool output tail | while the panel is open |

Streaming cost is proportional to new text. Each channel is a bounded queue
that coalesces under backpressure (latest state wins per key), and a
subscriber that falls behind receives a fresh snapshot instead of an
unbounded backlog, mirroring the server's own `ResetSnapshot` rule.

## Engine changes

Grouped by crate. Slice IDs refer to § Slices. Every protocol change bumps
`PROTOCOL_VERSION` with fixtures; every store change bumps the schema with a
migration test. Changes that land together share one bump where the slices
are ready together (decision #4 precedent).

### `qq-protocol`

- `Project`, `ProjectId`, `ProjectSummary`; project commands
  (`create_project`, `update_project`, `archive_project`) and
  `project_updated` events (K1).
- `WorkspaceKind::{Checkout, Worktree { branch, base, managed_root }}` and
  `WorkspaceSummary { project_id, kind, git: GitState }` (K3, K4).
- `CreateSession.isolation: Isolation::{Shared, Worktree { branch?, base? }}`
  (K4).
- `SessionSummary.attention` inputs that are not derivable today: awaiting
  input, goal state, last check result (K2). Read state per client (K2).
- `ChangeSet` per run and per session: files, insertions, deletions, and a
  bounded patch fetched on demand (K5).
- `ForkSession { from_run_id }` and `SteerRun.expected_run_id` precondition
  (K6).
- `Task`, `TaskId`, `TaskState`, `TaskResult`; task commands and events
  (K8).
- Machine-readable error reasons on command failures (`reason: &'static
  str` codes) so clients branch without parsing text (K2).

### `qq-core`

- **Project registry** (K1): projects as durable rows owning workspaces;
  configuration defaults per project resolved through `qq-config`.
- **Git state** (K3): a bounded, cancellable git status reader per workspace
  (branch, upstream, ahead/behind, dirty count), refreshed after mutating
  tool calls and on demand; off the Tokio workers.
- **Managed worktrees** (K4, ADR-0064): create under a server-owned root,
  run the project's setup commands as an approved, bounded process, record
  ownership, refuse removal while a session uses it or while it holds
  unpushed commits, garbage-collect settled ones. This replaces the
  *Intentionally Deferred* "git worktree isolation for editing subagents"
  entry in `tools.md` and lifts the "no worktree coordinator" statement in
  `architecture.md`.
- **Parallel writers** (K4): sessions in *different* workspaces may write
  concurrently; the existing rule that two writers never share a checkout
  stays, now enforced by workspace identity instead of by serializing all
  write children. A write child may be spawned with `isolation: worktree`.
- **Change tracking and restore points** (K5): per-run change sets from
  the tool layer's own write records plus a git diff against the run's
  start; restore points via the shadow-repository design in
  [`run-snapshots.md`](./run-snapshots.md), which this plan schedules.
- **Fork** (K6): a new session whose history is a prefix of another up to
  a run boundary, optionally in a new worktree.
- **Tasks and coordinator** (K8, ADR-0065): durable tasks; orchestration
  tools available only to a coordinator profile (`start_task`,
  `list_tasks`, `wait_tasks`, `message_task`, `cancel_task`,
  `review_task`); a concurrency cap per project and per server; task
  results summarized into the coordinator's context without importing
  worker transcripts.
- **Integration actions** (K9): commit, push, and pull-request creation as
  approval-classified actions with previews, using the existing shell and
  network policy rather than new effect classes where possible.

### `qq-server`

- Routes for the new commands, the change-set fetch, and the project and
  workspace catalog (S5 from the multi-surface plan, scoped by projects).
- Enrollment, TLS, and remote exposure (S2, S4) unchanged in design.
- A bounded per-server task scheduler admission that the coordinator and
  the user share, so a fan-out cannot exceed the server's agent cap.

### `qq-client`

- `servers` (W3) and `view` (C2) as above.
- `attention` (C1): one ranked status function used by every surface:
  `needs_approval` > `needs_input` > `failed` > `blocked` > `working` >
  `queued` > `checking` > `done_unread` > `idle` > `settled`.
- Optimistic intents keyed by `command_id`, already the TUI's model,
  shared by all surfaces.

### `qq-config`

- `projects` section (name, roots, defaults, `worktree.setup` commands per
  OS, `worktree.root`, concurrency caps) with trust rules identical to other
  project files; `qq config explain` covers every key.

### CLI and TUI

See § Surface parity. Each engine slice names its CLI or TUI entry point in
its acceptance.

## Desktop experience

The interface is the product. Phase D0 locks the information hierarchy in a
clickable prototype before production UI code is written; the layout below
is the starting brief for that prototype, not a frozen spec.

### Layout

```text
┌──────────────┬───────────────────────────────────────────┬─────────────────┐
│ Inbox     3  │ Fix token refresh   qq · wt/eng-123 · ●    │ Changes  Plan   │
│ Board        │ model · effort · mode                      │ Files  Output   │
│ ──────────── ├───────────────────────────────────────────┤                 │
│ ▾ qq   laptop│                                           │ +120 −8 4 files │
│   main       │  transcript (virtualized)                 │ src/auth.rs     │
│    ● Fix…    │  messages · one-line tool rows ·          │  ...hunk...     │
│   wt/eng-123 │  approval cards inline                    │  [revert hunk]  │
│    ◐ Add…    │                                           │                 │
│ ▸ web  home  ├───────────────────────────────────────────┤                 │
│ + Project    │ ▣ approve `cargo test`?  1 Once 2 Session │                 │
│              │ composer: queue / steer · @file · send ▾   │                 │
├──────────────┴───────────────────────────────────────────┴─────────────────┤
│ laptop ● live  home ● live · 4 working · 2 need you · $1.32 today          │
└────────────────────────────────────────────────────────────────────────────┘
```

- **Rail (left).** Inbox, Board, then projects grouped by server. A project
  expands to its workspaces; a workspace to its sessions, three by default
  with "show all". The rail collapses to icons. Server dots show connection.
- **Session (center).** A header of clickable chips (workspace and branch,
  model, effort, approval mode, budget). The transcript is turn-ordered per
  `docs/design/transcript.md`: messages in full, each tool call one line
  that expands, reasoning folded. A pending approval appears inline *and*
  pinned above the composer until decided, with keys `1`–`4` for once,
  session, workspace, deny.
- **Composer.** Drafts persist per session. While a run is active, Enter
  queues and `mod+Enter` steers (configurable inversion). `@` attaches
  files, `/` runs commands and skills, `mod+Shift+Enter` sends in the
  background and opens a new draft.
- **Context panel (right, `mod+\`).** *Changes* (the session's change set,
  per-turn filter, revert per hunk or file), *Plan* (goal, checklist, check
  result, budget left), *Files*, and *Output* (live tail of the running
  command, subscribed only while open).
- **Status bar.** Per-server connection, working and attention counts,
  spend today. Clicking any count filters the Board.

### Inbox

One list across every server of sessions and tasks whose attention is
`needs_approval`, `needs_input`, `failed`, `blocked`, or `done_unread`,
ranked by attention then age. Each row can be acted on in place (approve,
answer, retry, mark read) without opening the session. `mod+Shift+A`
jumps to the next item. The dock/taskbar badge is the count; native
notifications fire only for attention states, only when the window is
unfocused, and open the exact session.

### Board

The orchestration view: every task and root session across projects and
servers in a dense table.

| Attention | Project / workspace | Title | Activity | Time | Cost | Changes | Check |
| --- | --- | --- | --- | --- | --- | --- | --- |
| `◐ working` | qq / wt/eng-123 | Fix token refresh | `cargo test` 41 s | 12 m | $0.31 | +120 −8 | — |

Filters (needs me, working, done today, by project, by server, by
coordinator) and multi-select actions (stop, archive, open changes). A
coordinator's tasks group under it. Rows update in place; the board never
reflows while the user's pointer is over it.

### New task

`mod+N` opens a sheet: task text, project, workspace (*this checkout* or
*new worktree* with a suggested branch), model and effort, approval mode,
optional goal (check command and budget), and **fan-out**: several models or
`× N` attempts, each in its own worktree, grouped as one comparison on the
Board. Defaults come from the project; the sheet remembers the last choice.
Sending acknowledges immediately; worktree creation and setup show as task
states, not as a blocking dialog.

### Coordinator

A project can have one coordinator session, opened from the project header.
It looks like any session, plus a task strip showing its tasks and their
attention. The user talks to it; it starts, messages, waits on, reviews,
and cancels tasks; worker sessions remain openable and steerable directly.
Its approvals and its workers' approvals surface in the same Inbox.

### Review and land

From a finished task or session: the change set with per-file and per-hunk
revert, the check result, a one-paragraph summary, and actions *Fork from
here*, *Revert to before this turn* (files only, or files and history),
*Commit*, *Push*, *Open pull request*. Publishing actions show the exact
command and target and go through ordinary approval.

### Onboarding and settings

First launch: detect or start the local engine, sign in to a provider
(credentials go to the OS keychain through `qq-auth`), add a project folder,
trust its configuration (the same trust record `qq trust` writes). Each step
is skippable. Adding a remote server takes a pairing link or code (S2).
Settings edit the same `config.ron` layers the CLI reads and show where each
value comes from (`qq config explain`).

### Command palette and keyboard

Every action has a command id, a palette entry, and a rebindable shortcut.
`mod+K` searches sessions, tasks, projects, and servers; `>` lists actions;
`@` searches files in the current workspace. The app is fully usable from
the keyboard and every control has a visible focus state.

### Visual system

Dark and light themes following the OS, built from the TUI's `ink` palette
(ADR-0036) so the two surfaces feel like one product. One accent; semantic
colors for attention states, each paired with a distinct glyph so state
never depends on color alone. 13 px UI text, 14 px transcript, a monospace
face for code and paths, a 4 px spacing grid. Motion is off by default for
panels and session switches; only the working indicator animates, and it
respects reduced motion. Accessible primitives (Radix or Base UI), a single
icon set, code highlighting and diff computation in Web Workers.

### Performance budgets

Recorded in `benchmarks/perf/` under `desktop-*` keys and checked by the
`apps` gate on fixtures (not wall-clock assertions in unit tests):

| Budget | Target |
| --- | --- |
| Cold start to interactive, local sidecar already installed | < 1 s |
| Switch to an already-subscribed session | < 50 ms to first paint |
| Streamed token to pixels | < 1 frame at 60 Hz after IPC receipt |
| Patch apply p99 at 200 events/s across 16 streaming sessions | < 1 ms |
| Idle CPU with 10 running sessions, window hidden | < 1 % of one core |
| Rendered DOM nodes in a 10 000-item transcript | bounded by viewport |
| Memory after 1 h streaming 8 sessions | recorded baseline, no growth trend |
| Installer size | recorded per platform; regressions over 10 % fail |

## Surface parity

The engine capability is the unit of parity. A slice that adds one ships
its CLI entry point (and TUI where interactive) in the same slice or names
the follow-up slice that does.

| Capability | CLI | TUI | Desktop | Mobile (M) |
| --- | --- | --- | --- | --- |
| Projects | `qq project add\|list\|rm` | project picker | rail, settings | list |
| Managed worktree session | `qq --worktree [BRANCH]`, `qq run --worktree` | `/new --worktree` | New task sheet | — |
| Worktree lifecycle | `qq worktree list\|rm\|gc` | session list badges | workspace menu | — |
| Attention / Inbox | `qq inbox` (one-shot list) | NEEDS YOU group (exists) | Inbox, badges | Inbox, notifications |
| Change set | `qq diff --session ID [--run ID]` | changes pane | Changes panel | read-only |
| Fork, revert | `qq fork`, `qq revert` | `/fork`, `/revert` | Review actions | — |
| Fan-out | `qq run --fanout N\|--models A,B` | — (CLI is enough) | New task sheet | — |
| Tasks | `qq task list\|show\|cancel` | task strip | Board | list, approve |
| Coordinator | `qq coordinate` (TUI session in coordinator profile) | coordinator session | coordinator view | chat, approve |
| Goals (exist) | `qq goal` | goal form | Plan panel | status, pause |
| Remote servers | `qq pair`, `qq clients` (S2), `qq --server URL` | server switch | Servers | pair by QR |
| Commit / push / PR | ordinary shell under approval | same | Review actions | approve only |

Mobile is a companion: watch, approve, answer, steer, pause. It does not
start worktrees or review full diffs in its first release.

## Slices

Prefixes: `K` engine (`main`), `C` client core (`main`), `A` apps
(`feat/desktop`), `R` release. Multi-surface W3, S2, S4, S5, S6 keep their IDs
and acceptance in [`multi-surface-clients.md`](./multi-surface-clients.md).

| Slice | Phase | Goal | Inputs | Owned paths |
| --- | --- | --- | --- | --- |
| D0 | 0 | Clickable design prototype; design review sign-off | — | `apps/prototype/` on `feat/desktop` (deleted at A9) |
| A0 | 0 | `apps/` workspace, Tauri shell, CI matrix, `apps` xtask gate | ADR-0017, ADR-0018 | `apps/`, `xtask/src/apps.rs`, `.github/workflows/` (root request) |
| C1 | 1 | `qq-client::attention` ranked status | — | `crates/qq-client/src/attention.rs` |
| C2 | 1 | `qq-client::view`: view models, three patch channels, bounds | C1 | `crates/qq-client/src/view*` |
| C3 | 1 | Generated TypeScript types for view and intent types; drift check | C2 | `crates/qq-client/`, `apps/ui/src/gen/`, `xtask/` |
| K1 | 1 | Projects in store, protocol, config; `qq project` | — | `crates/qq-core/src/store*`, `crates/qq-protocol/`, `crates/qq-config/`, `crates/qq-server/`, `src/cli.rs` |
| K2 | 1 | Attention inputs on summaries; read state; error reason codes | — | `crates/qq-protocol/`, `crates/qq-core/src/sessions*`, `crates/qq-server/` |
| A1 | 2 | Local single-server desktop: rail, session, composer, approvals, palette, onboarding | A0, C2, C3 | `apps/` |
| A2 | 2 | Inbox, notifications, deep links, tray | A1, C1, K2 | `apps/` |
| K3 | 3 | Git state per workspace | K1 | `crates/qq-core/src/git*`, `crates/qq-protocol/` |
| K4 | 3 | Managed worktrees and parallel writers (ADR-0064); `qq worktree`, `--worktree` | K1, K3 | `crates/qq-core/`, `crates/qq-protocol/`, `crates/qq-server/`, `crates/qq-config/`, `src/` |
| K5 | 3 | Change sets and restore points (run snapshots) | K4 | `crates/qq-core/src/{changes,snapshots}*`, `crates/qq-protocol/` |
| K6 | 3 | Fork from a run; steer precondition | K2 | `crates/qq-core/src/sessions*`, `crates/qq-protocol/` |
| K7 | 3 | Fan-out: one request, N sessions in N worktrees, one group id | K4 | `crates/qq-core/`, `crates/qq-protocol/`, `src/cli.rs` |
| A3 | 3 | New task sheet, Board, Changes/Plan/Output panels, review actions | A2, K3–K7 | `apps/` |
| K8 | 4 | Tasks and coordinator profile (ADR-0065) | K4, K5, K7 | `crates/qq-core/`, `crates/qq-protocol/`, `crates/qq-harness/`, `src/` |
| K9 | 4 | Commit, push, PR actions with previews | K5 | `crates/qq-core/src/tools*`, `docs/design/tools.md` |
| A4 | 4 | Coordinator view and task strip | A3, K8, K9 | `apps/` |
| W3, S2, S4, S5, S6 | 5 | Remote readiness (multi-surface plan) | S1 | per that plan |
| A5 | 5 | Servers screen, pairing, cross-server Inbox and Board, offline states | A4, W3, S2, S4, S5 | `apps/` |
| A6 | 6 | Accessibility, theming, keybinding editor, settings parity | A5 | `apps/` |
| R1 | 6 | Signed installers, updater channels, release smoke on three OSes | A6 | `apps/desktop/`, `xtask/`, `docs/runbooks/` |
| A7 | 7 | Web build: core in a Web Worker, same UI | A6 | `apps/web/` |
| A8 | 7 | Mobile companion (Tauri mobile), QR pairing, foreground notifications | A7, S2 | `apps/mobile/` |
| A9 | gate | Merge `feat/desktop` to `main`; delete prototype; guide pages | R1 | `apps/`, `docs/guide/` |

### Acceptance highlights

Full acceptance is written into each slice's Linear issue when it starts;
these are the parts a reviewer must not let slip.

- **C2.** Replaying the protocol event fixtures produces the expected patch
  sequence (golden). Streaming 32 KiB into a 10 000-item transcript emits
  patches whose total size is within 2× of the streamed bytes. Each channel
  is bounded; a stalled subscriber receives one reset, not a backlog. No
  unbounded channel (lint).
- **C3.** Changing a Rust view type without regenerating fails `cargo xtask
  check`; the generated files are checked in.
- **K1.** Store migration test; projects survive restart; a project file in
  a repository is subject to trust like any project configuration.
- **K2.** A client that never loaded a session body can rank it correctly
  from the snapshot alone; read state is per enrolled client (or per
  loopback client id) and survives reconnect.
- **K4.** Two write sessions in two worktrees of one repository run
  concurrently and both complete; two write sessions in one checkout still
  serialize. Removal refuses a worktree in use or with unpushed commits;
  `gc` removes only worktrees QQ created (a test proves it refuses a
  directory it did not create, a `.git` directory, and a repository root).
  Setup commands run under approval policy with a timeout and bounded
  output. Crash during creation leaves no half-registered worktree.
- **K5.** A change set matches `git diff` for the run in fixture repos,
  including renames and binary files; revert to a restore point restores
  bytes exactly and never touches the user's `.git` state.
- **K7.** Fan-out of N acknowledges once, admits at most the server's cap
  at a time, and is idempotent under retry of the same `command_id`.
- **K8.** A scripted coordinator starts three tasks, waits, reviews one as
  failed and restarts it, and finishes with all results; a restart of the
  server mid-way resumes without duplicating a task. A coordinator cannot
  write files or spawn beyond the project cap. Worker transcripts never
  enter the coordinator's context beyond each task's bounded result.
- **A1–A5.** Playwright tests drive the built app against a real `qq serve`
  and the loopback fake provider from `xtask/src/smoke.rs`; no mocked core;
  no fixed sleeps. Each user-visible slice attaches `verify-qq`-style
  evidence (screens and the provider request log).
- **R1.** The performance budgets above are measured on all three
  platforms and recorded in the ledger.

## Phases

| Phase | Name | Exit gate |
| --- | --- | --- |
| 0 | Design and scaffold | Prototype covers Inbox, Board, Session, New task, Coordinator; design review signed off; empty Tauri app builds and runs in CI on macOS, Windows, Linux |
| 1 | Foundations | C1–C3, K1, K2 merged to `main` and merged forward; patch budgets met |
| 2 | Local desktop | A1, A2: the app can replace the TUI for daily single-project work on one machine |
| 3 | Parallel agents | Ten agents in ten worktrees of one repository on one machine, no checkout conflicts, Board responsive throughout; change review and revert work |
| 4 | Autonomous work | A coordinator completes a scripted multi-task project end to end with restart in the middle; one real multi-hour project recorded on an authorized subscription route |
| 5 | Remote | The laptop app drives a home server over Tailscale; network loss and recovery replay without gaps; revoking a client takes effect at once; security review of S2 passed |
| 6 | Release quality | Accessibility audit, signed installers, auto-update, three-OS release smoke, budgets recorded |
| 7 | Web and mobile | Approve, answer, and steer from a phone against a paired server |

Phases 1 and the remote slices (W3, S2–S6) are independent and may run in
parallel worktrees. Engine slices never wait for app slices.

## Branching

- `feat/desktop` is created from `main` and is the only integration branch
  for `apps/`. App slices branch from it and merge back by PR with the
  `apps` gate green.
- Engine and client-core slices (`K`, `C`, `W`, `S`) branch from and merge
  to `main` with the normal gates, so the TUI and CLI gain each capability
  immediately. `feat/desktop` merges `main` forward at least weekly and
  after every engine slice; it never carries engine changes of its own.
- The integration branch is merged to `main` at A9, after R1, as one PR
  whose diff is only `apps/`, its CI, and guide pages.

## Quality

- **Gates.** `cargo xtask check` gains an `apps` step that runs only when
  `apps/` or the generated types change: Rust fmt and clippy for the app
  workspace, `tsc`, lint, Vitest, Playwright against the fake provider,
  generated-type drift, and bundle-size budget. CI runs the app matrix on
  macOS, Windows, and Linux.
- **Tests.** UI tests exercise the real server and fake provider. View-core
  tests replay fixtures. Concurrency, cancellation, bounds, and
  replay/idempotency are tested wherever a slice touches them (AGENTS.md).
- **Code health.** No hand-written protocol or view types in TypeScript. One
  primitives package. Files over 600 lines need a stated reason in review.
- **Security.** Minimal Tauri capabilities and a strict CSP; the webview
  never sees credentials; keychain storage only; no secrets in URLs, logs,
  or crash reports; remote servers only through S2 enrollment and TLS or
  Tailscale (ADR-0015, S4).
- **Open source readiness.** CONTRIBUTING for `apps/`, a design-system page,
  screenshots generated from Playwright fixtures, reproducible builds from
  the repository alone.

## Non-goals

- A hosted control plane, relay, push service, or cloud execution. Work runs
  on servers the user owns; reaching them is direct (ADR-0009, ADR-0016).
- Multi-user tenancy or shared team workspaces.
- An embedded code editor or browser. The app opens files in the user's
  editor and links to pages in the user's browser.
- Plugins for the app. Extension stays in QQ's existing lanes (packs, MCP,
  skills).
- Several goals per session. Several pieces of work are several tasks.
- Recurring schedules and event triggers. A scheduler above QQ can drive
  `qq run` and the task commands; revisit after Phase 4 with evidence.

## Risks

| Risk | Mitigation |
| --- | --- |
| Engine scope grows faster than it can be reviewed | Engine slices are small, land on `main`, and each carries its CLI path; the app consumes only merged capabilities |
| Parallel writers expose new races | ADR-0064 keeps one writer per checkout; second review on K4 (touches `sessions/` and `store/`) |
| Disk use from many worktrees | Per-project cap, settle-time cleanup, `qq worktree gc`, size shown in the workspace menu |
| Coordinator spends without progress | Coordinator runs under a goal budget; tasks inherit bounded budgets; ADR-0054 progress reporting applies |
| Webview performance on Linux | Patch channels, virtualization, budgets measured on WebKitGTK from Phase 0 |
| Integration branch drifts | Weekly forward merges; engine changes forbidden on `feat/desktop` |
| UI quality regresses as features land | D0 prototype as reference, screenshot review on every `A` PR, accessibility checks in CI |
