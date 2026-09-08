# QQ Tool Execution And Security Design

## Purpose

This document defines how QQ agents read, search, and modify a workspace,
execute shell commands, and call MCP tools. It resolves the tool-execution
decisions deferred by `architecture.md` and `product.md`.

The design is ordered by the product priorities: speed and ease of use first,
with correctness, durability, and workspace safety as baseline constraints. A
tool layer that corrupts a checkout or loses history is a failure regardless
of latency, but every safety mechanism here is chosen to avoid long-held
locks, avoidable round trips, and interactive ceremony.

## The Tool Loop

A run is a loop owned by `qq-core`:

1. Assemble session context and request a model turn.
2. Stream text and tool-call requests as they arrive.
3. Persist each requested tool call, then resolve it: execute it, or wait for
   approval first when policy requires it.
4. Append tool results to context and request the next turn.
5. Repeat until the model finishes a turn with no tool calls, or the run is
   cancelled, interrupted, or fails.

The loop lives in `qq-core` next to `execute_run`, reusing the existing
cancellation watch, run permits, and persist-before-publish ordering. The TUI,
server, and direct CLI paths share it; no mode gets a parallel agent
implementation.

### Loop Bounds

"Repeat until no tool calls" needs a ceiling — a model that keeps calling
tools must not burn tokens forever. The loop is bounded three ways: tool
calls per turn (16), tool calls per run (64), and model turns per run
(65). The turn ceiling is one greater than the call ceiling so a run that
uses its last allowed tool call always gets a final model turn in which to
return an answer. The defaults are high enough that legitimate multi-step
work rarely notices them.

Hitting a ceiling ends the run with an explicit run outcome — not a silent
stop, and not a generic failure — so clients can render "turn limit
reached" and the user can continue with a follow-up prompt. The session
stays usable; the next run starts with a fresh budget.

### Agent Instructions

Tool declarations tell the model what it may call; they do not tell it
that it is an agent. `ModelRequest` carries a system-prompt field, and
`qq-core` owns a base agent prompt assembled per run: what the workspace
is, which tools are available, and the working conventions — read a file
before editing it, prefer `search` over guessing paths, cite paths
relative to the workspace root, establish observable completion criteria,
verify resulting state, and report remaining failures honestly. The prompt is
versioned in code, not user-editable configuration for now, so behavior
changes ship as reviewed diffs rather than config drift. Each provider maps the
field to its native system/instructions slot; no codec invents its own
preamble.

Before provider work, the same bounded blocking task that opens the workspace
selects root `AGENTS.md`, or root `CLAUDE.md` only when `AGENTS.md` is absent.
The selected regular UTF-8 file is capability-resolved, cannot escape through
a symlink, and is capped at 64 KiB. Because preparation selects at most one
root file, that individual limit is also the aggregate injected-instruction
limit. Missing both names is valid. The selected content joins the stable
system prefix; the prompt tells the model to inspect nested scopes root-to-leaf
and apply the same filename fallback before changing files below them.

Every prepared durable run records one all-or-none prompt identity before the
runtime is polled far enough to contact a provider: the nonzero base-prompt
version plus a validated SHA-256 hash of the prepared root path and bytes (or
the empty-selection hash). Historical runs and runs that fail before
preparation keep no identity. Nested instruction reads stay in the durable tool
transcript and do not retroactively change the pre-provider identity.

### Explicit Commands And Skills

Commands and skills are optional run guidance, not ambient policy. A leading
`/<name>` in the newest user message asks the shared `qq-core` runtime to load
one named Markdown document before contacting a provider. The original
invocation and its optional whitespace-separated remainder stay in the user
message so the selected document can interpret arguments without a second
client-side parser. A leading `//` escapes selection: QQ removes one slash and
sends the resulting literal slash-leading message without loading guidance.
That normalized text commits in the same transaction as `PromptQueued`; the
original command journal retains the escape marker so preparation does not
reinterpret it. Restart, event replay, snapshots, and follow-up context
therefore see the same prompt the first provider request saw.
Only an exact leading invocation is special; ordinary prompts never inject
skill bodies. Native `.qq/` roots and agent-pack roots are additionally
*disclosed*: the plan's compiled `SkillIndex` lists their names and YAML
front-matter descriptions in the system prompt, and the model may read one
body on demand with the `load_skill` tool. Compatibility roots (`.agents/`,
`.claude/`) are indexed for explicit invocation only and never disclosed. The
index holds at most 64 entries; a loaded body obeys the same bounds and
authority rules as an explicit invocation and is recorded in the run's
prompt identity.

The initial resolver searches repository-local sources in two precedence
tiers:

1. Native QQ sources: `.qq/commands/<name>.md` and
   `.qq/skills/<name>/SKILL.md`.
2. Compatibility sources, considered only when the native tier has no match:
   `.agents/skills/<name>/SKILL.md`, `.claude/commands/<name>.md`, and
   `.claude/skills/<name>/SKILL.md`.

Exactly one regular file must match within the selected tier. Multiple matches
are ambiguous and no match is unknown; both fail before provider work. Native
sources intentionally shadow compatibility sources, while a command and skill
in the same tier do not silently shadow each other. Names are 1--64 bytes,
start with a lowercase ASCII letter, and otherwise contain lowercase ASCII
letters, digits, `-`, or `_`. Client control names (every entry of
`qq_protocol::RESERVED_CLIENT_SLASH_COMMANDS`, such as `models`, `profile`,
`approval`, `skills`, `sessions`, `new`, `compact`, and `quit`) are reserved
and cannot name runtime guidance.

Authority follows command provenance rather than session ancestry. The
model-authored task that creates a child session cannot select guidance, while
an explicit user follow-up in that child may do so; child sessions remain
depth-capped and never gain `spawn_agent` from that selection.

Commands and skills use the same UTF-8 Markdown body contract. Their paths
supply name and kind; the only front matter interpreted is a `description`
line used for disclosure, so other foreign metadata remains ordinary guidance
text. A body is capped at 64 KiB,
resolved through the workspace capability, and rejected if it is not a regular
file, is invalid UTF-8, or escapes through a symlink. Supporting files remain
references only: loading a skill grants neither filesystem authority nor
permission to execute its scripts. The selected body joins the stable system
prefix after ambient workspace instructions, explicitly subordinate to those
instructions and to ordinary tool policy.

The durable run identity records the selected kind, name, repository-relative
source, optional declared version (absent in this initial format), and SHA-256
content hash. It also records hashes of the complete system prompt and ordered
provider-neutral tool declarations so evaluation artifacts remain explainable.
Clients may offer completion for discoverable names, but discovery, resolution,
loading, and rejection remain runtime behavior shared by direct, server, TUI,
and benchmark paths.

Agent packs add a third source: a pack selected by the session's profile
contributes its declared skill and command roots as `pack:<id>/...` between
the native and compatibility tiers, and prepends its persona to the system
prompt. Packs are directories with a `pack.ron` manifest discovered from the
global configuration directory and, for trusted projects, `.qq/packs/`; see
`docs/design/architecture.md`.

User-home, administrator-managed, and bundled roots are reserved follow-up
tiers. Reading the server process's home directory implicitly would make a
remote TUI mean something different from a direct run and would grant
host-level authority outside the selected workspace. Add such roots only as
explicit server-owned configuration with provenance and the same bounds; do
not infer them from the connecting client.

### Message And Content Model

Tool calls require structured message content. `qq_provider::Message` is a
role plus ordered content blocks:

- `Text { text }`
- `ToolCall { id, name, arguments }` (assistant turns)
- `ToolResult { call_id, content, is_error }` (returned turns)

`ModelRequest` carries the list of available tool declarations
(`ToolSpec { name, description, input_schema }`), and `ProviderEvent`
includes:

- `ToolCallStarted { id, name }`
- `ToolCallArgumentsDelta { id, json }`
- `ToolCallCompleted { id }`

Each provider codec maps these to its wire protocol internally. Provider
identity still must not branch in the request hot path; tool declarations are
compiled into the request the same way messages are. This content-block model
underpins everything else in this document; every codec carries contract
fixtures for it.

### Persistence And Replay

Tool calls follow the same authority rule as text: persist before publish.
Each call is a row keyed by run, call id, name, arguments, state
(`requested`, `awaiting_approval`, `running`, `completed`, `failed`,
`denied`, `interrupted`), and result. `SessionEvent` variants mirror the
state transitions so clients can replay a run and see exactly what the agent
did:

- `ToolCallRequested`
- `ToolApprovalRequested` / `ToolApprovalResolved`
- `ToolCallStarted`
- `ToolCallOutputDelta` (streamed shell output; batched like text deltas)
- `ToolCallFinished`

Recovery invariant: a tool call persisted as `running` without a persisted
result is never re-executed after a crash. `recover_interrupted_runs` marks it
`interrupted`; if the session resumes, the model sees an explicit interrupted
result and decides what to verify. Side effects are not idempotent, so replay
must never mean re-run.

Tool results can be large. Persist the full result up to a bounded size
(default 256 KiB per call, truncated with an explicit marker) and stream
deltas through the existing batching path so persistence latency stays off
the token hot path.

### Context Budget

Bounding each result is not enough; the accumulated context needs its own
bound. Every persist — message text, tool-call arguments, tool results —
runs a capacity check in the same transaction: the session's **assembled**
context (what the next run would actually send, after the compaction
cutoff and with stale read-only results pruned to stubs) must stay under a
fixed per-session cap (4 MiB), and a persist that would exceed it fails
the run. That is the backstop against unbounded growth, not window
management.

Result pruning is the first shedding mechanism (shipped compaction design;
bounds recorded under "Compaction Hardening" in
`docs/plans/terminal-bench-readiness.md`):
during assembly, read-only built-in results older than the last few model
turns are replaced by one-line stubs naming the tool, arguments, and size,
because the agent can re-derive them on demand. Mutating, shell, and MCP
outputs are never pruned — they are not re-derivable. The stored rows are
untouched; pruning is a property of assembly alone.

## Built-In Tools

The first tool set is small, executed in-process, and dispatched statically —
an enum, not a trait-object registry. This keeps per-call overhead near zero
and keeps the schema for each tool in one place:

- `read_file` — bounded read with offset/limit; records a content hash for
  the staleness guard below.
- `list_dir` — bounded directory listing.
- `search` — file-name and content search over the workspace, bounded result
  count.
- `edit_file` — exact-string replacement.
- `write_file` — full-file create or overwrite.
- `shell` — bounded command execution.

Read-only tools (`read_file`, `list_dir`, `search`) never require approval
inside the workspace and may execute concurrently. Everything else is a
mutating or externally visible tool and goes through policy.

## File References In Prompts

`@<path>` in a prompt is a client feature, not a tool. It is the user
putting a file into context: deterministic, immediate, no model round
trip, and no approval — the user's own action needs no gate. Agent-driven
discovery stays tool-based; `@` exists so the user never has to spend a
turn telling the agent to go read a file they already have in mind.

The client resolves references through the same capability containment as
the tools — an `@` reference cannot escape the workspace either — and
fuzzy completion reuses the `search` machinery rather than growing a
second index. The file's content attaches to the user message as a
content block, bounded and truncation-marked like a `read_file` result,
persisted like any other message content, and counted against the session
context budget.

Attaching a file also records its content hash in the session's
file-state map, exactly as `read_file` does. The read-before-write rule
is therefore already satisfied for pinned files: the agent may edit an
`@`-mentioned file without a redundant read, and the staleness CAS still
protects the apply.

## Safe File Editing

### Containment

Containment is a capability, not a path check. The workspace root is
canonicalized once and opened as a `cap-std` directory handle — the anchor
via `open_ambient_dir`, each component below it opened without following
symlinks. That handle is the only filesystem authority tools hold, and
every tool path resolves through it, so escape prevention is enforced by
the kernel at resolution time rather than by comparing strings before
opening. This is strictly stronger than canonicalize-and-prefix-check:
there is no TOCTOU window between a check and an open, and a symlink
inside the workspace that points outside it fails when the capability
resolves it, not after a race.

Tool paths must be relative to the workspace root; absolute paths are
rejected outright rather than re-rooted, so the model learns the real
addressing scheme instead of being silently corrected. Each path is then
canonicalized inside the capability: `..` traversal that escapes the root
fails there, and a resolved path that still carries a parent component is
rejected as a belt-and-suspenders check. `search` never follows symlinks
at all. Paths outside the workspace remain not an error class the agent
can approve its way through by default — wider access is an explicit
per-session grant, off by default.

### Edit Semantics

`edit_file` takes an exact `old_string`/`new_string` pair rather than a
unified diff. Exact-string replacement is what current models produce most
reliably, validation is trivial (the string is present exactly once or the
call fails), and a failed match returns a precise, retryable error instead of
a mis-applied hunk. `write_file` covers new files and full rewrites.

### Optimistic Concurrency, Not Locks

Safety across concurrent sessions in one workspace uses compare-and-swap, not
long-held locks:

1. `read_file` records the file's content hash in the session's file-state
   map.
2. `edit_file` and `write_file` (of an existing file) require a prior read in
   the same session.
3. At apply time, under a short per-workspace exclusive section, the current
   content is re-hashed. If it no longer matches what the session last read,
   the call fails with a stale-file error and the agent re-reads.
4. The apply itself validates `old_string` still matches, writes a temp file
   in the same directory, preserves permissions, and renames atomically.

The exclusive section covers only the hash-check-and-rename — microseconds —
so read-heavy parallelism across sessions is untouched and two writing
sessions interleave safely at file granularity. Semantic conflicts surface as
stale-file errors to the losing agent, which is the correct outcome: the
model re-reads and reconciles, exactly as a human would after a rebase.

This is the same progression `product.md` already commits to: concurrent
sessions share a checkout safely at file granularity now; editing subagents
get isolated worktrees later. Worktree orchestration stays deferred.

## Shell Execution

`shell` runs one command via `tokio::process::Command` with:

- Working directory pinned to the workspace (or a contained subdirectory).
- A default timeout (120 s, capped per call) that kills the whole process
  group, as does run cancellation.
- Bounded captured output (default 128 KiB, truncated head+tail with a
  marker), streamed to clients as `ToolCallOutputDelta` events through the
  existing batching path so long builds render live.
- No login/profile shell initialization on the hot path.

Shell is the one tool that cannot be contained by path checks — any command
can touch anything the server process can. Containment is therefore the
approval policy's job, and the honest framing is that `shell` approval trusts
the command. OS-level sandboxing (Landlock on Linux) is a worthwhile
hardening layer, but it is not a substitute for policy and is intentionally
deferred.

## Version Control

QQ ships no built-in git or jj tools. The model already speaks both
fluently through `shell`, and a `git_commit` tool would be a second,
worse-documented spelling of the same operation carrying its own approval
surface. First-class VCS support means the harness understands version
control, not that the model needs new verbs.

- **Read-only presets.** The default configuration layer's `policy`
  section ships shell grant prefixes for the interrogative subcommands:
  `git status`, `git diff`, `git log`, `git show`, `git blame`, and the
  jj equivalents (`jj status`, `jj diff`, `jj log`, `jj op log`,
  `jj show`). Under `ask` these run without prompting. They are ordinary
  config grants: visible in the same reviewable file, removable by a
  managed layer, matched at word granularity like every shell prefix.
- **Mutating commands follow ordinary shell policy.** `commit`,
  `checkout`, `rebase`, `restore` prompt under `ask` and are grantable
  like any other prefix; nothing special-cases them.
- **Outward-facing commands are never preset.** `git push`, `jj git
  push`, and anything else that publishes stays prompt-always unless a
  user writes the grant themselves. QQ does not make publishing a
  default.
- **jj is a policy entry, not a dependency.** jj users overwhelmingly
  run colocated repos, so git-shaped harness features (run snapshots,
  later worktree isolation) work for them unchanged. QQ takes no jj-lib
  dependency; revisit only if jj-native workspaces become a real ask.

The harness's own undo layer, run snapshots, is independent of the
user's VCS and planned in `docs/plans/run-snapshots.md`.

## External Tool Hosts

Anything that is not a built-in reaches the model through an
`ExternalToolHost`: a generation-stamped catalog, a bounded call with a
deadline and cancellation, typed failures (`timeout`, `cancelled`,
`unavailable`, `overloaded`, `invalid_result`, `refused`, `unknown_tool`,
`shut_down`), explicit readiness, and terminal shutdown. Two hosts exist: the
MCP registry below and an in-process `EmbeddedToolHost` (`ext__<host>__<tool>`)
that an embedding application registers closures on, with a frozen registry,
a concurrency permit, a per-call deadline, and argument (64 KiB) and result
(1 MiB) bounds. Both pass the same conformance suite. Hosts never retry
implicitly, and a host's effect hints are advisory: approval policy classifies
every call from the catalog's effect class, and every external tool (MCP or
embedded) is gated like a mutation — denied under read-only, held under ask and
supervised, and trusted under auto and full. A `read_only` hint only filters
the schema out of read-only requests; it never changes a decision.

Host tools are compiled into the plan's `ToolCatalog` at plan compile time,
not fetched per run. A tool is excluded, with a typed reason recorded in the
descriptor and the capability document, when its name is malformed or
duplicates another, its schema exceeds 16 KiB, its description exceeds 4 KiB,
the catalog already holds 512 tools, or external schemas already total 1 MiB.
A catalog with at most 24 external tools and 32 KiB of external schema is sent
whole on every request. A larger catalog is exposed progressively: requests
carry the built-ins plus `select_tools`, the system prompt carries a compact
index of external names and descriptions, and the model pins up to 32 tools
per run by keyword; pinned schemas join every later request in that run and a
recovered run re-pins from its transcript. Calling an unpinned external tool
is a tool error that points at `select_tools`.

## MCP

MCP is the primary external host. QQ does not grow a dynamic plugin API;
anything beyond the built-in tools arrives as an MCP server or through the
embedded host above.

- Servers are declared in configuration (global and per-workspace), with
  stdio and streamable-HTTP transports. Use the official Rust SDK (`rmcp`)
  with minimal features rather than hand-rolling the protocol.
- The QQ server owns one client connection per configured MCP server, shared
  by every session. Connections start lazily on first use (or eagerly at
  boot when configured), and tool schemas are fetched once into a numbered
  catalog generation that a `list_changed` notification, a reconnect, or a
  shutdown advances; a stale generation makes the plan stale, so the next
  load recompiles while active runs keep the catalog they were admitted with.
  Per-session connections would multiply startup cost and defeat connection
  reuse; a shared client keeps MCP calls as cheap as built-ins after the
  first use.
- MCP tools are namespaced `mcp__<server>__<tool>` and merged into the same
  declaration list, persistence, events, and approval flow as built-in
  tools. Clients render them identically.
- Concurrency: calls to distinct MCP servers proceed in parallel; calls to
  one server are limited by a small per-server bound so a slow server
  backpressures instead of queueing unboundedly.

MCP tools execute outside the workspace containment model, so they are
externally visible by default and require approval unless allowlisted.
Within a turn they execute in the sequential (mutating) path, never the
concurrent read-only path: an external call's side effects must not
interleave with other calls in the same turn.

### MCP Configuration

Servers are declared in the `mcp` section of the ordinary layered
documents, keyed by name. Names become the middle segment of
`mcp__<server>__<tool>`, so a name may not contain `__` (validation
rejects it) and the grammar stays unambiguous. Entries replace whole
declarations by name across layers; `Remove` deletes a server declared by
an earlier layer. Workspace declarations are sensitive operations behind
the same trust flow as providers, and remote configuration may not
declare servers at all.

```ron
(
    version: 1,
    mcp: {
        "executor": Stdio(
            command: "./executor.sh",
            args: ["--serve"],
            // Environment variables passed through to the child, which
            // otherwise starts from a cleared environment plus PATH/HOME.
            env: ["EXECUTOR_API_KEY"],
            eager: true,                  // connect at startup, not first use
            allow: ["execute", "skills"], // per-server tool allowlist
        ),
        "linear": Http(
            url: "https://mcp.linear.app/mcp",
            bearer: Env("LINEAR_TOKEN"),  // sourced like every other secret
            call_timeout_seconds: 60,     // default 60, max 600
            max_concurrent_calls: 4,      // per-server bound, default 4
        ),
    },
)
```

One deliberate convenience: the per-MCP-server tool allowlist lives on
the server's own `mcp` entry, next to the declaration it scopes, even
though the `policy` section also accepts workspace grants. The entries
are folded into the resolved grant set as exact names
(`mcp__<server>__<tool>`) — the same set the approval flow consults, and
the same set a managed `deny_tools` list can filter.

## Approval Policy

Approvals are explicit policy, not hidden behavior, and they are first-class
protocol objects so every client — TUI, CLI, or future web — uses the same
flow.

Each session has an approval mode:

- `read-only` — only read-only built-ins and allowlisted read-only MCP tools
  execute; everything else is denied without prompting.
- `ask` — workspace-contained edits, writes, shell, and non-allowlisted MCP
  calls each request approval.
- `auto` (default) — workspace-contained edits, writes, and MCP calls execute
  without prompting; shell commands matching the allowlist or carrying no
  dangerous pattern execute; destructive or externally visible shell commands
  ask (or are adjudicated by the configured reviewer model).
- `supervised` — every mutating, shell, and MCP call is held and adjudicated
  by the reviewer model regardless of grants; a reviewer denial is final and a
  reviewer escalation reaches the human. Only spawned write children run here;
  a client cannot select it directly.
- `full` — everything executes without prompting.

The allowlist is deliberately simple: exact commands or command prefixes
(`cargo test`, `git status`), plus per-tool grants for MCP. No pattern DSL
until real use demands one.

### Grant Lifetimes

A grant answers "may this run without asking", and the same grant shapes
carry three lifetimes:

- **Once** — approve a single call; nothing is recorded.
- **Session** — approve-for-session records a grant consulted by every
  later policy check in that session. Shell grants are command prefixes
  matched at word granularity (`cargo test` covers `cargo test -p x`,
  never `cargo testify`); other tools are granted by exact name. A
  prefix never extends over shell control characters — a command
  containing `|`, `;`, `&`, redirection, or substitution is more than
  one program, so it matches only a grant equal to the exact string.
  The check is quote-blind on purpose: it errs toward prompting.
- **Workspace** — the grants a user always wants live in the `policy`
  section of configuration, in the same layered documents as everything
  else. Same shapes, longer lifetime: exact tool names, shell command
  prefixes, and per-MCP-server tool allowlists. Config grants merge into
  the session's grant set at session creation, with the existing config
  layer precedence, so a managed source can constrain what a workspace
  may allowlist.

Workspace grants are written, not invented: the approval prompt grows an
"always allow in this workspace" choice that promotes the grant into the
workspace config document. Trust decisions land in the same reviewable
file users already edit — no hidden allowlist store, no second syntax.

### Workspace Grant Configuration

Catalog exposure is configured separately with optional
`policy.exposed_tools: ["read_file", "search"]`. Lists intersect across
layers and with the selected profile/pack policy; absent adds no restriction
and `[]` removes all tools. At most 1024 distinct exact names may appear in
one declaration. Built-in names and MCP syntax are checked during config
loading, and admitted MCP tool membership is checked during compilation.
Restricting exposure grants no authority and needs no trust approval.
Neither approval grants nor `--approval full` restore an excluded tool.
Supervisors select the profile using `qq run --profile <name>`.

Grants live in the `policy` section of the ordinary layered documents.
Any non-remote source may declare the two grant shapes; the constraint
fields stay managed-only:

```ron
// Workspace or user configuration.
(
    version: 1,
    policy: (
        allow_tools: ["edit_file", "mcp__executor__execute"],
        allow_shell_prefixes: ["cargo test", "git status"],
    ),
)

// Managed configuration constrains what lower layers may grant.
(
    version: 1,
    policy: (
        deny_tools: ["mcp__executor__execute"],
        deny_shell_prefixes: ["git push"],
    ),
)
```

- **Shapes and grammar.** `allow_tools` entries are exact tool names
  (built-in names, or `mcp__<server>__<tool>` with the server segment
  obeying the MCP name rules). `allow_shell_prefixes` entries are word-
  granularity command prefixes: non-empty, no control characters, no
  surrounding whitespace. Duplicates within one list are rejected;
  across layers the sets dedupe naturally.
- **Layering.** Later layers extend the accumulated set, and
  `Remove("name")` deletes a grant declared by an earlier layer — the
  same removal-marker idiom `mcp` and `providers` use.
- **Managed constraint.** `deny_tools` and `deny_shell_prefixes` are
  managed/MDM-only and filter lower-layer grants out of the effective
  set rather than erroring. Tool denies match exact names, including
  folded MCP allowlist entries. A denied shell prefix removes every
  grant it covers at word granularity *and* every broader grant that
  would cover the denied commands (`cargo` denied removes `cargo test`;
  `cargo test` denied also removes a bare `cargo` grant, because a
  config-layer filter cannot partially subtract a broader grant).
- **Trust.** Workspace-declared grants are sensitive operations behind
  the same trust flow as MCP declarations, and remote configuration may
  not declare them at all.
- **Promotion.** The approval prompt's workspace-lifetime choice appends
  the grant to `.qq/config.ron` by targeted text insertion — comments
  and formatting survive — with an atomic temp-and-rename write that
  must reparse before it lands. The complete read-modify-write and trust
  update are serialized across processes by a workspace-keyed file lock in
  QQ's private data directory. Promotion refuses grants the managed
  layer denies. Because the write is the user's own decision, the file's
  new trust digest is recorded immediately — but only when the file was
  already trusted (or had no sensitive content) beforehand, so promotion
  never launders trust for unreviewed declarations.
- **Resolution.** The effective configuration exposes the resolved grant
  set (declared grants plus folded MCP allowlists, minus denies), which
  seeds each session's grant set at creation with the existing config
  layer precedence.

Flow: when policy requires approval, the runtime persists and publishes
`ToolApprovalRequested` and the run stays active but waiting — it holds its
run permit, other sessions are unaffected, and cancellation still works. A
client responds with an idempotent `RespondToolApproval` command
(approve once, approve-and-allowlist for the session, approve-and-promote
for the workspace, or deny). Denials are
returned to the model as tool errors, not run failures, so the agent can take
another path. Non-interactive automation chooses its policy up front via
flags (`--approval`, plus `--allow-tool` and `--allow-shell` allowlists that
answer a held call with a session grant); a headless run with `ask` semantics
and no attached client fails the approval after a bounded wait rather than
hanging forever.

Approval requests carry enough to decide without leaving the client: the
resolved path and a diff preview for edits, the exact command and cwd for
shell, the server, tool, and arguments for MCP.

## Parallelism

- **Across sessions:** unchanged — runs are already concurrent under bounded
  permits. The tool layer adds no global locks; the only cross-session
  exclusion is the per-workspace microsecond apply section.
- **Within a turn:** when a model emits several tool calls in one turn,
  read-only calls execute concurrently under a small bound; mutating and
  shell calls execute in request order. Results are appended to context in
  request order regardless of completion order so context assembly stays
  deterministic.
- **Persistence:** all tool events flow through the existing single-writer
  store worker with the existing batching, keeping SQLite off the streaming
  hot path.
- **MCP:** shared clients, per-server bounds, parallel across servers.

## Failure-Path Testing

The failure paths carry direct tests — containment escapes, stale-file
conflicts, approval denial and idempotent retry, timeout and cancellation
kills, crash recovery marking `running` calls interrupted — and tool-call
dispatch overhead has a benchmark.

## Intentionally Deferred

- Git worktree or sandbox isolation for editing subagents.
- OS-level shell sandboxing (Landlock/seccomp).
- Approval pattern languages or per-path ACLs.
- A plugin API beyond MCP.
- Cross-workspace tool access as anything but an explicit grant.
