# QQ Desktop & Mobile: Fleet Orchestration Plan

**Status:** Proposed. Runs alongside [core stabilization](README.md), with the
gates described below; it does not lift the feature freeze on its own.

## 0. Decisions and the main principle

| Topic | Decision |
|---|---|
| UI stack | Shared Rust app core. **GPUI** for desktop, thin **SwiftUI/Compose** shells over **UniFFI** for mobile. |
| Core freeze | Work runs in parallel with stabilization, but gated. Multi-agent *write* orchestration waits until the stabilization rows for permissions, containment and edits pass. |
| Topology | One **hub per project**. Workers are plain `qq` daemons. |
| Scale | About 300 agents across 3–8 hosts. |
| Network | Tailnet or LAN with pinned TLS. Push goes direct to APNs/FCM. |
| Users | Single operator. Every command records which client sent it. |

**Principle: rebuild what exists so TUI, CLI, desktop and mobile are equal clients of one per-user daemon.** Do not build the desktop around today's process model. Concretely:

- The runtime lives in a library and runs in exactly one place: the **`qq` daemon**.
- Every surface reaches it through the same protocol and the same client crates.
- Any surface can start the daemon. None of them owns it.
- Closing a client never affects work.
- You can use the TUI while someone else uses the desktop (or you switch), against the same sessions, with the same capabilities.

**Orchestration principle (unchanged):** the hub never runs an agent loop. A **Task Attempt** is an ordinary root session with a Goal, running in an isolated execution checkout on some worker. QQ's goal loop, approvals, compaction, durable store and idempotent commands do the work. The hub adds placement, dependencies, verification, integration and attention.

---

## 1. Why today's shape blocks four equal surfaces

All of these were checked against `main` at `b11092e8`.

| Coupling | Evidence | Consequence |
|---|---|---|
| The TUI embeds the server when none is running. Quitting the owning TUI cancels **every** run, including runs started by other clients. | `src/main.rs:826-896`, `985`; `qq-core/src/sessions/runtime.rs:1349-1377` | Desktop and TUI cannot coexist safely. |
| Composition (RuntimeFactory, loader, reviewer, grant/goal authority, catalog, trust, command-worker client) is in the `qq` binary crate, about 4k production lines in `src/runtime.rs`. ADR-0050 (move it to `qq-harness`) is still Proposed. | `src/runtime.rs:236-3316`; `qq-harness/src/lib.rs:2` | Nothing else can host the runtime, and composition code is duplicated across `ask`, `run`, `serve` and the TUI. |
| `qq run` opens the store directly and fails with `StoreBusy` (exit 4) while any server runs. It relies on `SessionRuntime` methods that have no route. | `main.rs:555-562`; `headless.rs:529,592,714,784,1305,1322` | CLI automation and the desktop exclude each other. |
| Trust, auth login, config read and edit, `@`-mention resolution and completion, goal defaults, provider remedies and the default model are computed **locally in the client process**. Trust requires owning the server. | `main.rs:763-815`, `904-919`, `1803-1966`; `qq-tui/src/terminal.rs:297-326` | A remote or mobile client cannot onboard, trust a project, log in or use `@`. |
| `qq-tui` depends on `qq-core` (mentions, `MAX_GRANT_BYTES`). Slash commands are tied to crossterm. Queued drafts are client-local, so other surfaces can't see them and they are lost on quit. | `qq-tui/Cargo.toml`; `commands.rs:14`; `qq-client/src/state.rs:909-958` | TUI behavior can't be shared with other surfaces. |
| `qq-client` handles one workspace on one server. `SessionStore` is `!Sync`. `TuiClient` takes a `PathBuf` and is tied to TUI ownership semantics. | `interactive.rs:33-48`, `384`; `state.rs:13` | No multi-server or multi-workspace model, and no FFI. |
| The daemon's config depends on its launch directory and environment (`approval_timeout`, `QQ_*`, provider keys, `PATH`). | `main.rs:651-661`; `runtime.rs:933`, `2219-2270` | A daemon started from Finder behaves differently from one started in a shell. |
| Version handling is fragile. The protocol must match exactly. Store migrations only go forward and an old binary fails with an untyped error. The command worker is locked to one build (`IncompatibleBuild` blocks shell tools). Desktop and CLI binaries share all locks and directories. | `qq-protocol/src/local.rs:36`; `schema.rs:395`; `command_owner/client.rs:176-181` | A desktop-bundled `qq` and a CLI-installed `qq` break each other. |
| Remote access: loopback only, no TLS, single rotating token, no enrollment (ADR-0015 Proposed). | `qq-server/src/lib.rs:664-704` | No remote clients. |
| Event streams are per workspace only: no feed across workspaces, a cap of 64 subscriptions, and `ToolCallOutputDelta` is sent to every subscriber. axum HTTP/2 is accidental (it arrives through the AWS SDK features). | `lib.rs:73-74`, `1097-1116`; `Cargo.toml:55` | Clients can't follow a fleet, and the mobile data budget is blown. |
| A session has no execution root other than its workspace path. A worktree registered as its own workspace needs trust again (trust is keyed by path), takes a permanent workspace row (rows are never deleted; the cap is 1024), adds one SSE stream per worktree, and multiplies the plan-cache key. | `sessions/runtime.rs:271-278`; `loader.rs:1451`; `commands.rs:776` | Worktrees-as-workspaces doesn't scale and is unsafe. This needs a core change. |
| Limits across the whole store: `MAX_COMMANDS`=100k receipts for the store's lifetime, never trimmed. 512 sessions per workspace, children included, with no archiving (ADR-0038 Proposed). | `sessions.rs:388`; `commands.rs:409-424` | A fleet hits these hard ceilings within weeks. |
| A held approval keeps its run slot; `max_active_runs`=8 and it can't be configured. Nothing measures more than 8 streams. Every tool-output delta is an fsynced event row. | `scheduler.rs:87,139`; `runtime.rs:545`; `xtask/src/perf.rs` | Throughput limits at 50–100 agents per host. |

---

## 2. Target architecture: one daemon, four equal clients

```
 qq (TUI)   qq run/ask/goal/... (CLI)   QQ.app (GPUI)   iOS / Android (SwiftUI/Compose)
    │                │                      │                  │ UniFFI
    └──── qq-app (surface-neutral app model) ┴── qq-app-ffi ────┘
                         │
                    qq-client (transport, supervisor, multi-server registry, pinning)
                         │  HTTP/1.1+2, SSE  (loopback token locally; enrolled credential remotely)
                 ┌───────▼────────────────────────────────────────────┐
                 │ qq daemon  = qq-server (transport) + qq-host        │
                 │   qq-host: composition (factory, loader, reviewer,  │
                 │            trust, auth, config, catalog, mentions,  │
                 │            command worker, env policy, lifecycle)   │
                 │   qq-core: sessions, runs, goals, tools, checkouts  │
                 │   qq-fleet: hub (projects, tasks, scheduler, inbox) │
                 └─────────────────────────────────────────────────────┘
```

### 2.1 Crate responsibilities after the refactor

| Crate | Responsibility | Change |
|---|---|---|
| `qq-protocol` | Wire types only | + bootstrap, trust, auth, config, mentions, feed, checkout, enrollment, fleet types; tolerant versioning |
| `qq-core` | Runtime | + execution checkouts, parked holds, retention, server-feed emission; no surface code |
| **`qq-host`** (rename `qq-harness` and finish ADR-0050) | Composition: `RuntimeFactory`, `RuntimeLoader`, reviewer, grant/goal authority, model catalog and discovery, trust, server-side auth flows, config inspect/edit, mention resolution, command-worker client, env policy, `Daemon::start(paths, env) -> DaemonHandle`, and the headless driver core | Moved out of `src/` |
| `qq-server` | HTTP/SSE transport, auth, TLS, routes | + new routes, TLS, enrollment, feed, explicit HTTP/2 |
| `qq-fleet` | Hub (§6) | New |
| `qq-client` | Transport, `SessionClient`, connection supervisor (start-or-attach, backoff, routes, pinning), `ServerRegistry` | `TuiClient` replaced by surface-neutral `WorkspaceSession` |
| **`qq-app`** | The surface-neutral app model **every interactive surface uses, including the TUI**: a store keyed by `(ServerId, WorkspaceId)` per server, reducers (the existing `qq-client` state), a command registry for slash commands and actions without key types, attention/inbox model, pickers' data, mention completion through the daemon, client cache, view-model snapshots and deltas | New; the TUI migrates onto it |
| `qq-tui` | Rendering and terminal input only | Drops `qq-core`; consumes `qq-app` |
| `qq-desktop` | GPUI rendering | New; outside `default-members` |
| `qq-app-ffi`, `apps/ios`, `apps/android` | Mobile shells | New |
| `src/` | argv parsing and output formatting only | Shrinks; every subcommand is a `qq-client` call, or a `qq-host` call in isolated mode |

Dependency rule, enforced by an `xtask lint` rule:
- Surfaces (`qq-tui`, `qq-desktop`, `qq-app*`) never depend on `qq-core`, `qq-host`, `qq-config` or `qq-auth`.
- Only the daemon does.

### 2.2 Daemon lifecycle

**Start-or-attach.** One function, `qq_client::ensure_daemon(binary)`, used by every surface:
1. Discover `server.ron` and health-check it.
2. If nothing answers, spawn `<binary> daemon run --detach`.
3. Wait on a readiness handshake (a pipe or socket write after the listener binds).
4. Attach.

The reconnect resolver uses the same function, so a crashed daemon is restarted by whichever client notices first.

Who starts it:
- **Bare `qq`** (TUI): start-or-attach. It never embeds the server.
- **Desktop:** start-or-attach using its **bundled `qq` binary**. The desktop doesn't link the runtime. Reasons:
  - One runtime implementation.
  - `current_exe` dispatch for `_command-owner`/`_command-exec` keeps working.
  - A UI crash never kills agents.
  - The daemon upgrades independently of the UI.
- **Service mode** (optional): `qq daemon install|uninstall` registers launchd, systemd `--user` or a Windows service for always-on hosts and workers.

**Lifetime.**
- The daemon outlives every client.
- It handles SIGTERM and SIGINT with a graceful drain: stop admitting work, let runs reach a turn boundary or a timeout, then exit. Goals resume after restart.
- Optional idle-exit only when there are no clients, runs or goals and the setting is enabled. It is off for service mode.
- Commands: `qq daemon status|stop|restart|logs`, plus a `POST /v1/daemon/shutdown` route that only the loopback token can call.

**Upgrades.**
- `server.ron` records the daemon's binary path, build and protocol range.
- A client with a newer compatible binary attaches and shows "daemon vX, client vY".
- `qq daemon upgrade` (also a desktop button) restarts the daemon onto the newest binary with a drain.
- An incompatible client gets a typed `DaemonIncompatible{daemon, supported}` error and an upgrade offer. No silent failure.

**Command worker.**
- Its state directory is scoped to the daemon instance and build.
- It exits when its coordinator daemon exits or after it has been idle.
- Mixed builds can never block shell tools again.

**Environment policy.**
- The daemon's config is **independent of how it was launched**. Server controls such as `approval_timeout` come from global and managed config, never from the launch cwd.
- On start, the daemon captures a login-shell environment (`$SHELL -lc env`, with a timeout). Config `daemon.env` can pass or deny specific variables.
- Clients never forward their environment.
- `qq doctor` reports which environment the daemon is using.

**Store safety.**
- A typed `StoreSchemaNewer{found, supported}` error.
- The data directory is pinned explicitly for sandboxed or Store builds, so desktop and CLI always share one store.
- Desktop builds are distributed outside the macOS sandbox, because the sandbox would redirect the directories.

**Isolated mode.** `qq run --isolated` (and CI) runs `qq-host` in-process with a private data directory. This is the **only** in-process path left, and it uses the same `qq-host` code as the daemon.

### 2.3 Parity: everything surfaces need goes through the protocol

| Capability | Today | Target route or command |
|---|---|---|
| Bootstrap (default model, remedies, goal defaults and ceilings, approval timeout, profiles, skills, packs, commands) | Computed locally (`TuiModelState`) | `POST /v1/bootstrap{workspace_id}` → `WorkspaceBootstrap` |
| Trust | Local, owner only | `pending_trust` in bootstrap; `ResolveTrust{digest, scope: Persist\|Daemon}` |
| Provider auth | Local CLI, keyring | `auth/status`, `auth/set_key`; device-code login `start`/`poll` run in the daemon. Browser-callback OAuth is client-assisted: the daemon issues the URL and receives the callback on its own loopback; remote clients use device code. |
| Config | Local inspect; promotion only | `config/inspect` (sources, show, explain); `config/edit` limited to scoped, trust-checked keys |
| `@` mentions | Client filesystem + git | `mentions/complete{prefix}`; resolution moves into `SubmitPrompt` on the daemon, where `InputPart::Mention` expands on the server |
| Files and diffs | None | `files/tree`, `files/read`, `checkout/diff`, `git/status` (P4) |
| MCP inspect, doctor, init, packs | Local | `mcp/inspect`, `doctor/report`, `workspace/init`, plus packs in bootstrap |
| Queued drafts | Client-local, ≤8, lost on quit | Become **server follow-ups** using the existing queue, ≤16 per session, editable and removable through `edit_follow_up` and `remove_follow_up`. Every surface sees them. |
| Headless driver needs (`run_snapshot`, `goal_owns_run`, `goal_pending_runs`, `pause_goal_if_active`, recovered-command wait) | In-process only | Routes; `qq run` becomes a `SessionClient` consumer |
| Unattended approval policy (`--allow-*`, unattended deny) | Client-side in `headless.rs` | A session-level `UnattendedPolicy` the daemon enforces, so goal attempts and `qq run` behave the same |

**Stays client-local by design:** layout, fold/focus, threadline, theme, keybindings, unread markers, composer text.

**Multi-client control** (resolves product.md's open decision): every command is idempotent and attributed. Approvals are first-wins: later responses get the existing resolution. Steering and cancel are last-writer. No locks or "controller" role.

### 2.4 Surface matrix

| | TUI | CLI | Desktop | Mobile |
|---|---|---|---|---|
| Starts daemon | yes | yes (`qq daemon`, `run`, `goal`…) | yes (bundled `qq`) | no (remote only) |
| Sessions, transcript, approvals, steer | yes | `run`, `goal`, `session` subcommands | yes | yes (lightweight transcript) |
| Multi-server | `--server <profile>` | `--server` | yes | yes |
| Projects, tasks, inbox, fleet | `qq fleet` / Inbox pane (later) | `qq fleet …` | full | inbox, launcher, review-lite |
| Trust, auth, config | through daemon routes | through daemon routes | through daemon routes | auth and trust only |

---

## 3. Domain model (added to CONTEXT.md)

Existing terms keep their meanings. Keep "_Avoid: Swarm_"; parallel work is a **Task Graph**.

- **Daemon:** the one per-user process hosting a Server. Clients start it or attach to it; none of them owns it.
- **Fleet:** the enrolled servers under one operator.
- **Hub:** the server that owns a project's orchestration state.
- **Worker:** a server that hosts attempts. A hub is a worker for itself.
- **Project:** hub-owned. It contains:
  - repos, identified by normalized remote URL or root commit;
  - a brief (markdown);
  - an environment recipe (setup, include list, ports);
  - defaults for model, profile, budget and unattended policy;
  - a task graph.
- **Placement:** a project's workspace on one worker, made of a canonical clone and a checkout pool.
- **Task:** objective, kind (`Change` | `Research`), repo, acceptance check, budget, priority, dependencies, labels.
- **Attempt:** one execution of a task: a root session with a Goal, in its own Execution Checkout. Best-of-N means N attempts.
- **Change Set** (from a `Change` attempt): branch, base revision, diff stats, per-turn checkpoints.
- **Report** (from a `Research` attempt): typed final output.
- **Integration Queue:** per project. Rebase, verify, fast-forward.
- **Inbox:** server-side Needs Attention across the fleet.

Hierarchy: `Project → Task → Attempt (root Session + Goal, Execution Checkout, Worker) → Runs → in-run read delegation (unchanged)`.

The 300-agent scale is **task-level parallelism**. In-run delegation stays small: at most 3 concurrent, depth ≤3.

---

## 4. Core changes for execution checkouts (`qq-core::checkout`)

An Execution Checkout is a **session attribute under one workspace, not a new workspace**. This avoids re-trusting each worktree, keeps the workspace-row and plan-cache counts flat, and needs one stream per project.

- `create_session{checkout: Isolated{base_ref, branch}}` → `session_checkouts(session_id, root, base, branch, state)`.
- `RuntimeLoadRequest` carries `execution_root`.
- Tools use it:
  - File tools: `Workspace::open(execution_root)` with the same protections.
  - Shell: `cwd` resolves under `execution_root`.
  - Native scopes are keyed by `(workspace, execution_root)`, and a read-write scope also grants the worktree's gitdir inside the main `.git`.
  - Edit `apply_lock` is already keyed by path, so attempts don't contend.
- **Config and trust come from the workspace's trusted digest at session creation and are pinned into the plan.** They are never read from the worktree. An agent that edits `.qq/` in its branch cannot widen its own authority.
- **Plan cache key** uses the workspace and config digest, not the checkout.
- **Pool:**
  - Canonical clone plus `git worktree` pool, with N warm worktrees that have the setup script already run.
  - Gitignored files listed in `.qq/worktree-include` are copied in.
  - Each checkout gets its own port range and environment (`QQ_PORT_BASE`).
- **Checkpoints:**
  - Hidden refs `refs/qq/checkpoints/<session>/<turn>`, written through a temporary `GIT_INDEX_FILE` and captured asynchronously at turn boundaries.
  - These give per-turn diffs and revert. Revert requires an isolated checkout.
- **GC:**
  - A cap per placement.
  - Snapshot to a ref, then delete.
  - Never delete active or pinned checkouts.
- **Git in checkouts:** commit is allowed by the attempt's `UnattendedPolicy` grant (prefix `git commit`, `git add` scoped to the checkout). Push and PR are a separate Repository Publication policy, with credentials kept outside the agent (per stabilization).

---

## 5. Scale changes per host (target: 50–100 agents)

Every change is evidence-led and measured with a new `xtask perf` 100-stream scenario. Results are reported, not used as blocking gates.

| Change | Why |
|---|---|
| `daemon.max_active_runs` configurable, defaulting from cores and memory, and reported in capabilities | The hard-coded 8 (32 counting child-depth pools) |
| **Parked holds:** a run waiting on approval or `ask_user` releases its admission permit and gets it back with priority when resolved | Held runs pin slots indefinitely today |
| **Tool output moves out of the event log** (ADR, amends ADR-0003 narrowly). Shell and exec output goes to an append-only chunk table, written by the same store worker and persisted before it is published. The workspace event carries only `ToolCallOutputAdvanced{bytes, lines}`, coalesced to ≤4 Hz. Clients that expand a tool row subscribe to `GET …/tool-calls/{id}/output` (replay plus live). | The single biggest event and fsync volume, sent to every subscriber |
| Adaptive output batching: the 8 ms window grows toward about 50 ms as active runs increase. Any change to the durability level needs its own ADR plus evidence. | Group commit under 100 streams |
| Shared provider limiter in `qq-provider`: concurrency plus a token bucket per endpoint and credential, fed by 429 and `Retry-After` (provider still owns retry, per ADR-0005). Explicit reqwest `http2`. | The real ceiling at 300 agents is provider limits |
| Blocking file-tool permits scale with available parallelism; the plan-cache cap is reviewed after the key change | Fixed 8 and 16 today |
| **Retention** (amend ADR-0038): archive settled attempts (excluded from the 512 count); cascade deletion; workspace removal; receipts trimmed after an idempotency window (default 7 days); `MAX_COMMANDS` becomes a bound on *pending* work, not a lifetime count | Store-wide permanent ceilings |
| Explicit axum `http2` with ALPN under TLS | Remove the accidental cleartext h2; multiplex streams |

---

## 6. Hub orchestration (`crates/qq-fleet`, inside the daemon, enabled per project)

- **State:** separate `fleet.sqlite3` with its own writer thread. It follows persist-before-publish and the cursor scheme.
- **Workers:** the hub reaches each worker (itself included, over loopback) with `qq-client`. It subscribes to that worker's **server feed** and issues idempotent commands with `command_id = H(attempt_id, step)`.
- **If the hub fails, work continues.** Attempts are goal sessions on workers. On restart the hub reconciles from worker snapshots.

**Task states:** `Draft → Ready → Placing → Running ⇄ NeedsAttention → Verifying → Review → Integrating → Done`. Terminal alternatives: `Failed | Blocked{reason} | Cancelled | Superseded`. Review is skipped when the project uses `auto_integrate` and verification passed.

**Scheduler:** a pure function `step(&mut State, Input) -> Vec<Effect>`.
- Indexed structures: ready queue `BTreeMap<(priority, enqueued), TaskId>`, dependency counters, worker capacity. Transitions cost O(log n).
- Order: persist, then publish, then run effects on bounded pools per resource class (git, provider, verify).

**Placement:**
- Filter first, on eligibility: repo present or provisionable, labels, health, protocol compatibility, containment capability for `Change` tasks. A worker without the repo **refuses**; there is no fallback to another checkout.
- Then score: free slots, warm checkout available, locality to dependency branches, recent failure rate.

**Admission:**
- Slots per worker.
- **Per provider account across the fleet**: concurrency plus a token bucket, throttled by AIMD on the 429 counts workers report.
- Concurrency and daily spend per project.
- Weighted fair queueing across projects.

**Failure handling:**
- A worker that disconnects becomes `Suspect`; its attempts become `Unknown` and are reconciled when it reconnects.
- After a grace period, attempts are re-placed as new attempts. This is safe because each has its own checkout.
- Denied effects never replay automatically.
- Budgets roll up attempt → task → project. Hitting a ceiling pauses work and creates an inbox item; there are no silent resets.

**Attempt creation:**
1. `create_session{checkout}`.
2. `set_goal{objective, check, budget}` plus the project's `UnattendedPolicy`.
3. A prompt containing the task brief, the project brief, and dependency Reports or base branches.

Progress comes from the goal checklist and `RunActivity`, without a model summarizing it.

**Verify and integrate:**
- The goal check runs in the checkout. An optional read-only review attempt (`SessionPurpose::Audit`) checks the diff.
- Integration Queue: rebase onto `integration`, re-check, fast-forward.
- On conflict, a `Resolve` task is created and placed where both branches exist.
- Publication (push or PR) is explicit policy per project.

**Best-of-N:** attempts run in parallel and are compared on diff, check and review verdict. The winner is integrated. The others become `Superseded`: snapshotted, then their checkouts are freed.

**Coordinator** (optional): a hub-local root session with durable project tools: `create_tasks(graph)`, `list_tasks`, `wait_tasks`, `read_report`, `review_change_set`, `steer_task`, `cancel_task`. In plan mode it proposes a graph; you approve or edit it, then it runs.

**Research swarms ship early:** read-only `Research` tasks on shared checkouts need no worktrees.

**Clients talk only to the hub.** The hub proxies detail routes to the owning worker as raw bytes (`/v1/servers/{id}/…`). Later, the desktop can learn direct routes to workers.

---

## 7. Protocol changes

Each change ships with regenerated fixtures and a `PROTOCOL_VERSION` bump. They are grouped by phase.

1. **Versioning (ADR):**
   - `ServerInfo{protocol_version, min_client}` with a range check.
   - Events and snapshots decode tolerantly: unknown variants become `Unknown{kind}`. Commands stay strict.
   - Feature detection through capabilities.
   - Typed incompatibility errors.
2. **Parity routes:** everything in §2.3, including daemon lifecycle and `UnattendedPolicy`.
3. **Remote:**
   - ADR-0015 accepted: pairing code → `qqc1_` credential; enroll, clients, revoke.
   - TLS with an `rcgen` self-signed certificate; its fingerprint goes in the `qq://pair?host&code&fp` QR code and is pinned by a custom rustls verifier.
   - A non-loopback bind requires TLS.
   - `client_id` is recorded on receipts and events.
4. **Streams:**
   - `GET /v1/feed`, a summary-only feed across workspaces with a server-level cursor and a retention window. Resuming past the window returns `410`, then the client re-snapshots. Events: `SessionSummaryChanged`, `AttentionChanged`, `RunSettled`, `CapacityChanged`, `WorkspaceChanged`, derived from the existing summary-carrying events.
   - `?session=` filter on workspace streams.
   - Tool-output subscription per tool call (§5).
5. **History:** session-list cursor, messages before a cursor, rename, archive.
6. **Checkout:** `create_session.checkout`, `ChangeSetSnapshot`, files, diff (base↔head, turn N↔M), `git/status`, attachment upload.
7. **Fleet** (hub): projects, tasks, attempts, inbox, workers, project feed, proxy, device registration for push.

---

## 8. UX: exceptions, not activity

Every view answers one of three questions: **what needs me, what is moving, what is done.** The shared `qq-app` model gives the same lists and actions on every surface.

- **Inbox (home):**
  - Contents: approvals, questions, failed checks, ready reviews, blocked tasks and budget stops.
  - Sorted by unblock value: how many downstream tasks are waiting.
  - Keys: `j/k` move, `a` approve, `A` approve for session, `d` deny, `r` reply, `e` snooze, `⏎` open. The goal state is empty.
- **Pulse:**
  - One painted grid with a cell per running attempt, grouped by project. Color shows state; brightness shows recency.
  - Header: tasks per hour, spend per hour, critical-path ETA, capacity per worker.
  - Hovering a cell shows one line ("cargo test · 2m14s · 7/9"). No transcripts.
- **Project:** a board (Ready, Running, Review, Done) that toggles to a graph view with the critical path highlighted, plus brief, defaults, environment and placements.
- **Task:**
  - Attempts side by side.
  - Change Set diff with check results.
  - A Merge-readiness panel.
  - Transcript folded by default; tool output loads only into rows you expand.
- **Review:** virtualized, highlighted diff. Inline comments are sent to the attempt as a steer. Approve sends the change to the integration queue.
- **Launcher (`⌘K` / TUI `:`):**
  - Text plus `repo= on= model= n= budget= check=`.
  - `on=` shows where the work will run before you send it. `n=3` is best-of-3. `plan:` goes to the coordinator.
- **Rules:**
  - Every action is ≤2 keys.
  - Commands are optimistic and reconcile with receipts.
  - Stale cached data is marked.
  - Notifications only for needs attention, review ready, failed, and project complete. They are batched.
- **Mobile:** inbox, approvals, launcher, status, review-lite, approve and merge. A Live Activity per active project. Opens from the cache.
- **TUI:** gains the Inbox and multi-server workspace switching through `qq-app`. Fleet views beyond the Inbox can come later.

---

## 9. App core and performance budgets

**`qq-app` core:**
- Its own tokio runtime decodes and reduces state off the UI thread.
- The UI receives `Arc` snapshots plus keyed deltas, coalesced per frame.
- Keyed `IndexMap` stores with caps: summaries per server, cold-session `RunStats` evicted (this fixes a growth leak in today's reducer), warm transcripts LRU.
- Markdown, tree-sitter and diff work runs off-thread with LRU caches.
- A client cache in bounded SQLite.

**GPUI desktop:**
- Pin the GPUI revision and use gpui-kit lists, tables and editor.
- Only visible rows subscribe to updates. Pulse is a single custom element.
- Packaged on its own (`.app`, AppImage, MSI) with the bundled `qq`.

**Mobile:**
- `qq-app-ffi` exposes `subscribe(view) → callback diffs`, `command()` and `cancel()`. The store is wrapped in a `Mutex` and returns owned view-models.
- Networking stays in Rust (reqwest, rustls, `rustls-platform-verifier`, pinned fingerprint).
- The hub sends APNs (HTTP/2 + ES256 JWT) and FCM v1 directly. Payloads carry ids and counts only. The app catches up from its cursor when it comes to the foreground.

**Budgets** (reported, not gates):
- Cold start to cached UI: under 300 ms desktop, under 500 ms mobile.
- Optimistic UI response: under 16 ms.
- Loopback command ack p99: under 30 ms.
- Transcript first paint: under 100 ms.
- 300-cell Pulse: under 8 ms per frame.
- Desktop memory: under 400 MB at 300 agents.
- Client traffic: under 20 KB/s at 300 agents.
- Daemon start-or-attach: under 150 ms when the daemon is already running, under 1 s for a cold start.

---

## 10. Phases

Each PR follows the repo's loop: a failing test, the smallest fix, `cargo xtask check`.

### P1 — One daemon, four equal clients (local)
This is mostly refactoring and deletion, so it fits stabilization's "less code" row. Coordinate the `headless.rs` and goal changes with the stabilization row on task survival.

1. ADR-0050 accepted. Move composition from `src/` into `qq-host`, so `src/` shrinks to argv and rendering.
2. Daemon lifecycle:
   - `qq daemon run|status|stop|restart|upgrade|install`;
   - detach and readiness handshake; SIGTERM drain; start-or-attach in `qq-client`;
   - bare `qq` stops embedding the server and the `owns_server` path is deleted;
   - command worker scoped to the daemon instance; environment and cwd policy.
3. Versioning ADR, tolerant decoding, typed store and daemon incompatibility errors.
4. Parity routes (§2.3). Server-side follow-ups replace client drafts. `UnattendedPolicy`.
5. `qq run`, `qq goal`, `qq config`, `qq auth`, `qq trust`, `qq doctor` become daemon clients. `--isolated` mode added.
6. `qq-client` generalized: `WorkspaceSession`, `ServerRegistry`. `qq-app` extracted, and the TUI migrated onto it. The TUI drops `qq-core`. Add the surface-dependency lint.
7. Run in parallel: a **GPUI spike** measuring a 300-cell grid and a 10k-row list.

**Exit criteria:**
- The TUI and a scripted second client drive the same session at once.
- Quitting either client never cancels anything.
- `qq run` works while the TUI is open.
- Trust, auth and `@` work through daemon routes alone.
- A newer client attaches to an older compatible daemon, verified by fixtures.

### P2 — Remote and multiple servers
- Enrollment, TLS and pinning, non-loopback bind, actor attribution.
- `/v1/feed`, session-filtered streams, history pagination, rename and archive, explicit HTTP/2.
- `--server <profile>` for the TUI and CLI.

**Exit:** the TUI on laptop A pairs with and drives the daemon on host B over a tailnet. Restarting B keeps the credential valid.

### P3 — Desktop app (single and multiple servers, no fleet yet)
- Server profiles, workspaces, session list, transcript, composer, approvals.
- Per-server Inbox, settings for trust, auth and config, daemon management.

**Exit:** desktop budgets are met against 100 fake sessions, and the TUI runs alongside the desktop.

### P4 — Execution checkouts, change sets, review
Gated on the stabilization rows for edits and permissions.
- §4 in full, plus file and diff routes and the review UI.

**Exit:**
- Two attempts edit one repo concurrently.
- Per-turn diffs and revert are correct.
- An agent's `.qq/` edits in its branch don't change its authority.
- GC never deletes active work.

### P5 — Scale per host (§5)

**Exit:** 100 concurrent fake-provider attempts on one host complete. Ack p99, commit latency and fsync rate are reported, with no limit errors.

### P6 — Hub and fleet orchestration (§6)
- An ADR amends Multi-Server Overview and retires the non-goals "distributed execution workers" and "autonomous multi-agent editing".
- Research tasks may land after P2. Change tasks need P4.
- Desktop gets the fleet views: Inbox, Pulse, board, graph, task, best-of-N.

**Exit:** deterministic `xtask smoke fleet`, using 4 isolated homes on loopback fakes:
- a 300-task graph completes with correct ordering and integrations;
- killing a worker or restarting the hub produces no duplicate effects;
- a 429 storm throttles placement.

### P7 — Mobile
- `qq-app-ffi`, iOS and Android shells, QR pairing, APNs/FCM, Live Activity.

**Exit:** an approval pushed to the phone is resolved there and the run resumes. Cold launch from cache meets its budget.

### P8 — Autonomy
- Event triggers (CI via `gh` ETag polling, review comments, cron).
- Project memory.
- Multi-repo projects.
- Direct desktop-to-worker routes.
- FTS5 search.

---

## 11. Verification
- Fully offline against `qq_provider::test_support` fakes. No fixed sleeps; tests use controlled clocks.
- Table tests over the scheduler's `step` function.
- Protocol fixtures for the previous and current versions, plus a test that an old client decodes a new server's events.
- `xtask smoke` scenarios:
  - **two-clients:** TUI and scripted client on one daemon;
  - **daemon-lifecycle:** start-or-attach, drain, upgrade, crash and restart;
  - **fleet.**
- Headless GPUI view-model tests; core tests for the FFI layer; XCTest and Compose UI smoke tests.
- Each phase proves itself by driving real binaries with `verify-qq`.

## 12. Documentation updates (as code makes them wrong)
- `CONTEXT.md`: §3 terms; amend Multi-Server Overview, Client and Delegation.
- `product.md`: interaction modes (daemon), non-goals, parallel direction.
- `architecture.md`: composition, daemon lifecycle, remote.
- `protocol.md`: fix the existing drift in the route list and examples.
- Guides: `headless.md`, `tui.md` (quitting no longer cancels runs), and new `daemon.md` and `fleet.md`.

## 13. Risks
- **GPUI churn:** pin the revision and keep view-models in `qq-app`.
- **Moving tool output out of the event log** is the riskiest core change. It needs an ADR, a narrow amendment to ADR-0003, and replay-equivalence tests.
- **Store fsync at 100 streams** is unmeasured. P5 measures it before any change to durability.
- **Provider limits** bound real throughput. Report tasks completed per hour, not agent count.
- **Review bandwidth:** the levers are inbox ordering, auto-integration when verification passes, and coordinator-enforced small scopes.
- **Daemon environment capture on macOS and Windows:** test it per OS, and make `qq doctor` show what the daemon captured.
- **Push setup:** needs an APNs key and an FCM account configured on the hub.
- **Containment:** containment is per OS, Linux first. Workers report whether they have it, and placement filters `Change` tasks on it.
