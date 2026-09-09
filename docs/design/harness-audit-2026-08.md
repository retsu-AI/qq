# Agent Harness Reference Audit — 2026-08

Status: reference. This document records the static, read-only audit of four
local agent-harness snapshots (Codex, OpenCode, Pi, fx) and the Hermes product
boundary that informed
[`docs/plans/speed-first-extensible-agent-harness.md`](../plans/speed-first-extensible-agent-harness.md).
It is research, not a plan: it does not change as tasks ship. The "QQ" column
in the inventory tables reflects QQ as of the audit (2026-08); consult the
plan's task index and [`architecture.md`](./architecture.md) for current
behavior.

Extracted from the plan on 2026-09-08 without content changes.

## Audit Method

The design is based on independent, read-only source audits of four ignored
local reference snapshots under `.source/` plus the current QQ implementation
and plans.

| Project | Inspected identity | Files | Snapshot manifest SHA-256 | Evidence boundary |
| --- | --- | ---: | --- | --- |
| Codex | workspace and SDK versions `0.0.0` / `0.0.0-dev` | 6,883 | `4f780e9d53ea4ef0c5f20ce307e9b89005b4515e622a22b0d08e9bb7c4b82f17` | No nested Git metadata; 141 Rust workspace member paths |
| OpenCode | version `1.18.25` | 6,543 | `4fbf5422a7bc33150d6d79bc70afcc9950d6ff1e730ee95bda8bd68ed935a39f` | Bun monorepo with current V1 and incomplete experimental V2 runtimes |
| Pi | coding-agent version `0.84.4` | 1,410 | `d5a87ac144bf16d1c8cfecac60f5cf3d9c40479da0e057e84452070b1421dc4e` | TypeScript monorepo; shipped coding runtime differs from newer durable-harness work |
| fx | runtime version `0.0.7` | 800 | `88732147452e5aa7164ed11bdef218bfcbe1f3020e1339a1e2667e03a4c7aa84` | Experimental Zig runtime; manifests contain placeholder versions |

The snapshots have no nested Git repositories, so QQ's enclosing commit must
not be attributed to them. The audit was static: their build, startup, binary
size, memory, and benchmark claims were not independently executed. The local
`.source/` directory remains ignored and is not a repository dependency.

The manifest hashes above identify the exact audited local trees. They hash
the sorted sequence of each regular file's SHA-256 and relative path; they do
not include empty directories or file modes. They detect snapshot drift but do
not replace missing upstream revision provenance or make the ignored sources a
shipping dependency.

Key evidence anchors retained for a future re-audit are:

| Project | Local snapshot anchors |
| --- | --- |
| Codex | `codex-rs/core/src/session/turn.rs`, `codex-rs/thread-store/src/store.rs`, `codex-rs/core/src/session/mod.rs`, `codex-rs/sandboxing/src/manager.rs`, `codex-rs/hooks/src/lib.rs`, `codex-rs/app-server/README.md` |
| OpenCode | `packages/core/src/event.ts`, `packages/core/src/session/input.ts`, `packages/sdk-next/src/opencode.ts`, `packages/plugin/src/index.ts`, `packages/opencode/src/server/server.ts`, `SECURITY.md` |
| Pi | `packages/agent/README.md`, `packages/agent/docs/harness.md`, `packages/agent/src/harness/agent-harness.ts`, `packages/coding-agent/src/core/session-manager.ts`, `packages/coding-agent/docs/extensions.md`, `packages/protocol/README.md` |
| fx | `src/core/agent/stream_provider.zig`, `src/core/session/session_log.zig`, `src/core/subagent/domain.zig`, `src/builtins/tools.zig`, `src/core/permissions/permissions.zig`, `sdk/README.md` |

The comparison records implemented code separately from experimental,
incomplete, or marketing-only behavior. Removed compatibility flags and
unimplemented roadmap claims are not counted as available features.

## Current QQ Baseline

QQ currently provides:

- one binary with TUI, `ask`, durable `run`, `serve`, configuration,
  authentication, organization, and workspace-trust commands;
- OpenAI Responses, OpenAI Codex subscription, Anthropic Messages, Google
  GenerateContent, xAI Responses and Chat, Bedrock ConverseStream, and Mantle
  Responses, Chat, and Anthropic protocol support;
- LiteLLM and compatible custom deployment recipes using supported protocols;
- model catalogs with pricing, context, output, reasoning, and cache metadata;
- built-in `read_file`, `list_dir`, `search`, `edit_file`, `write_file`, and
  one-shot `shell` tools, plus conditional `spawn_agent` and namespaced MCP
  tools;
- capability-scoped filesystem access, no-follow path handling, read-before-
  write hashes, atomic replacement, bounded output, process-group cleanup, and
  cancellation;
- read-only, ask, automatic, and full approval modes with scoped grants,
  managed denies, previews, and model-reviewed held tools;
- MCP over stdio and Streamable HTTP with trust, authorization, cached
  connections, schema caching, list-change handling, backoff, deadlines, and
  bounds;
- SQLite WAL persistence through a dedicated worker, idempotent commands,
  authoritative events, replay cursors, snapshots, recovery, cancellation,
  deletion, pruning, and compaction;
- automatic and manual context compaction and stale read-only result pruning;
- renewable internal 256-tool execution slices with durable checkpoints;
- durable read-only child sessions with atomic creation, parent ownership,
  depth and concurrency caps, cancellation, recovery, worker-model selection,
  and cost roll-up;
- repository-root instructions and explicit repository-local commands and
  skills with persisted content hashes;
- an HTTP/SSE server, authenticated client, TUI session management, live
  approvals, model selection, cost/context display, and reconnect/replay; and
- durable JSONL traces, Harbor/ATIF export support, provider canaries, provider
  compilation benchmarks, a synthetic tool-loop benchmark, and manual TUI
  performance cases.

The gaps recorded at the original 2026-08 audit, with their current status:

| Area | Original gap | Status (2026-09-04) |
| --- | --- | --- |
| Streaming persistence | Text batches reconstructed context and grew strings by concatenation | Resolved by R4 (linear chunks); one fsync per batch remains → D2 |
| Reasoning persistence | Deltas committed independently | Resolved by R4 (bounded batches) |
| Store scheduling | Control always preferred; full output queues polled | Resolved by R4 (wake-driven fairness); 14 `sleep(1 ms)` overload loops remain → D8 |
| Context planning | Fixed byte budget and trigger | Resolved by R5 (provider-aware admission, occupancy reuse) |
| Resolved model state | Runtime loading dropped effective limits | Resolved by R5 and H2 (`ResolvedModel` in the plan) |
| Run limits | No core-owned outcome | Resolved by R5 and H3 (`RunLimits`, `budget_exhausted`, capabilities) |
| Terminal | `shell` is one-shot without a durable handle | Open; owned by R6 |
| Search/edit | Literal scan and exact replacement | Open; owned by R6 tournament |
| Retry ownership | Provider and core retries can amplify | Confirmed at up to 24 sends per turn → D3 (H14) |
| Approval identity | (found 2026-09-04) `ext__` tools classified `Unknown` and execute in every mode | P0 → D4 (H13) |
| Event fan-out | (found 2026-09-04) each subscriber re-reads and re-serializes every event | → D1 (H15) |
| Evaluation | No complete useful-result latency gate | Partially resolved by H0 gates; end-to-end quality gate remains H12 |

One footprint issue mattered for embedders at the time of the audit:
`qq-provider` depended unconditionally on the AWS SDK family. H1 resolved it by
feature-gating that family inside the existing provider crate rather than
splitting providers into crates.

## Cross-Project Feature Inventory

`Yes` means the capability is substantiated in the inspected implementation.
`Partial` means experimental, incomplete, unsafe for backend use, or present
only through an example or alternate runtime. `No` means it was not found as a
first-class capability.

### Runtime And Durable State

| Capability | QQ | Codex | OpenCode | Pi | fx |
| --- | --- | --- | --- | --- | --- |
| Shared core across interfaces | Yes: direct, TUI, server, durable headless | Yes | Partial: V1 and incomplete V2 | Partial: shipped loop and incomplete new harness | Yes: CLI, TUI, ACP, SDK |
| Streaming text, reasoning, and tools | Yes | Yes | Yes | Yes | Yes |
| Cancellation | Yes | Yes | Yes | Yes | Yes |
| Active-run steering | Partial: queue or cancel | Yes | Partial | Yes | Queued at model boundary |
| Structured output | No first-class contract (HC3 proposed) | Yes | Yes | Provider-dependent | Yes |
| Multimodal input | Text-only protocol | Images and media | Files and images | Images and vision | Native vision; SDK lacks images |
| Bounded provider retries | Pre-stream only | Yes | Yes | Yes | Yes, with delivery certainty |
| Durable sessions | SQLite authority | JSONL plus SQLite projection | SQLite; stronger V2 event store | Shipped synchronous JSONL | Checksummed framed event log |
| Persist-before-publish | Yes, fail-closed | Attempted; append errors can be swallowed | Partial: implemented only in experimental V2 | No strong invariant | Strong local-log semantics |
| Idempotent command admission | Yes | Limited | Partial: implemented only in experimental V2 | No | No inbox equivalent |
| Cursor replay | Yes, SSE cursors | Subscription and replay | V2 event replay | Branching history | Tape and session recovery |
| Session lifecycle | Create, resume, delete, prune, compact | Resume, fork, archive, rollback, delete | Resume, fork, revert, import/export | Resume, fork, clone, labels, export | Resume, migrate, recover, undo |
| Compaction | Automatic/manual and stale-result pruning | Local and remote summaries | Summary, pruning, context epochs | Compaction and branch summaries | Fast deterministic extractive summary |
| Crash-safe long-run checkpointing | Renewable internal slices | No strong execution checkpoint | V2 explicitly incomplete | No | Recovery checkpoints and `/continue` |
| Sub-agents | Durable bounded read-only children | Full-session tree with caps | Foreground/background tasks | Example child CLI processes | Durable persistent children |
| Durable child recovery | Yes | Partial | No for background jobs | No | Yes |

### Providers, Tools, Extensions, And Security

| Capability | QQ | Codex | OpenCode | Pi | fx |
| --- | --- | --- | --- | --- | --- |
| Provider coverage | OpenAI, Anthropic, Google, xAI, Bedrock, compatible gateways | Responses-compatible, Bedrock, Ollama, LM Studio | More than 20 providers | Roughly 30-provider catalog | Gateway, Codex, Grok |
| Existing-protocol custom deployments | Yes | Responses-compatible only | Yes | Yes | No generic endpoint registration |
| New provider protocol seam | Rust `Provider` boundary | Compile-time/core work | Core work for new wire protocol | Strong SDK boundary | Fixed enum/set and core edits |
| Built-in coding tools | Read, list, search, edit, write, shell | Broad coding, media, web, and agent set | Broad coding, web, LSP, and task set | Read, shell, PowerShell, edit, write, grep, find, list | Sixteen tools including terminal, web, vision, skills, MCP, sub-agent |
| Parallel tools | Read-only groups bounded; mutations serial | Parallel; no obvious per-turn semaphore | Supported | Unbounded fan-out risk | Leading read-only group; thread per call |
| Persistent terminal or PTY | No | Yes | Yes | No | Excellent durable terminal model |
| Bounded tool output | Yes | Generally | Yes with spill storage | Yes | Yes with opaque retrieval handles |
| Web and vision | External/provider dependent | Native | Native | Provider/extension dependent | Native |
| MCP client | Stdio and Streamable HTTP | Rich | Rich | Extension only | Stdio, HTTP, legacy SSE |
| Skills and commands | Repository-local | Rich discovery/packages | Local/remote skills and commands | Strong package/resource UX | Multiple compatible roots and install |
| External executable addons | MCP | MCP and hosted tools | MCP and JS/TS plugins | Extensions | MCP |
| In-process extensions | No general plugin API | Compile-time Rust contributors | Broad sequential JavaScript hooks | Broad TypeScript extension API | Compile-time typed hooks |
| Context/memory extension | No first-class seam | Compile-time contributors | Partial plugin/context mechanisms | Strong resource/context hooks | Budgets but no external provider seam |
| Approval policy | Read-only, ask, auto, full, scoped grants | Rich typed policy | Rich UX rules | Trust-oriented | Ask, auto, yolo, exact targets |
| Capability filesystem containment | Yes | Yes | No | No | Policy only |
| Native OS process sandbox | No | Seatbelt, seccomp/bubblewrap, Landlock, Windows | No | No | No substantiated backend |
| Bounded scheduling/backpressure | Mostly; store fairness remains | Several unbounded queues | V2 SSE bounded, global pubsub unbounded | Several unbounded fan-outs | Bounded data; steps default unbounded |
| Dynamic package installation | No | Plugins, skills, marketplaces | npm/local plugins and skills | Strong | Skills only |

### Interfaces And Operations

| Capability | QQ | Codex | OpenCode | Pi | fx |
| --- | --- | --- | --- | --- | --- |
| Interactive TUI | Yes | Yes | Yes | Rich | Differential renderer |
| Direct automation | `ask`, durable `run`, JSONL | Exec and JSONL | Run and JSON | Print, JSON, RPC | Ask, JSON, replay |
| Long-running HTTP/SSE server | Yes | No stable HTTP daemon | Yes | No | No |
| ACP | No | No primary ACP surface found | Yes | Custom RPC | Yes over stdio |
| OpenAI-compatible API | No | No | No | No | No |
| Native client | Rust `qq-client` | Internal Rust crates | Generated TypeScript client | TypeScript SDK | Native core plus JS bridge |
| Python or TypeScript SDK | No | Both, with performance caveats | Yes | Strong embedded SDK | Experimental Node/Wasm SDK |
| Layered configuration | Yes | Extremely broad | Extremely broad | Yes | Yes |
| Provider authentication | OAuth, keys, organizations | Broad | Broad | Broad | Three provider-specific flows |
| Server authentication | Local-instance bearer | Trusted-local app server | Optional Basic Auth | Not applicable | ACP stdio |
| Fleet observability | Partial | Strong OTLP/runtime metrics | OTLP and logs | Telemetry-oriented | Local trace and stats |
| Evaluation/performance gates | Partial | Weak benchmark coverage | Partial | Provider/eval ergonomics | Strong eval, startup, and size discipline |
| Messaging, cron, or voice | No | No harness-plane feature | No harness-plane feature | No | No |

## Detailed Reference Findings

### Codex

Codex provides:

- a streaming turn loop with reasoning, messages, tools, retries,
  cancellation, steering, review, diffs, goals, memories, web, images, and
  pre- and mid-turn compaction;
- Responses-compatible providers, Bedrock and Mantle variants, Ollama, LM
  Studio, model catalogs, reasoning controls, remote compaction, cached HTTP
  and WebSocket transport, preconnect, sticky routing, and request
  compression;
- a very broad tool catalog including PTY execution and stdin, patching,
  media, web, MCP, planning, permissions, skills, plugins, goals, and agent
  control;
- start, resume, fork, archive, delete, rollback, compact, name, list, and
  replay session behavior;
- canonical JSONL history plus a rebuildable SQLite projection;
- copy-on-write context history, tool-result pairing, token estimation, local
  and remote compaction;
- full-session sub-agents with depth/concurrency limits and root-scoped
  control;
- the strongest inspected sandbox set: macOS Seatbelt, Linux seccomp plus
  bubblewrap, legacy Landlock, and Windows restricted tokens;
- rich approval and permission profiles;
- MCP, declarative plugin bundles, compile-time Rust contributors, hooks,
  skills, marketplaces, app-server, exec-server, TUI, CLI, TypeScript and
  Python SDKs; and
- extensive OTLP metrics including startup, TTFT, tool, process, persistence,
  and memory signals.

QQ should borrow transport prewarming, immutable tool snapshots, sandbox
adapters, lifecycle vocabulary, hook trust hashes, root-scoped agent control,
and metric coverage.

QQ should reject Codex's 141-member decomposition, broad compile-time
extension and optional code-mode footprint, unbounded queues,
subprocess-per-turn TypeScript design, trusted-local administrative APIs on an
external surface, and fail-open persistence error handling.

### OpenCode

OpenCode provides:

- a current streaming coding loop with retries, tool use, structured output,
  compaction, doom-loop handling, and serialized sessions;
- broad provider, model, pricing, limit, variant, and cache catalogs;
- shell, read, glob, grep, edit, write, patch, task, web, question, todo,
  skill, LSP, custom JavaScript/TypeScript, plugin, and MCP tools;
- SQLite-backed sessions and an experimental V2 event store that atomically
  commits events, aggregate sequence, and projections before publishing;
- an idempotent durable prompt inbox and session-local serialized execution
  coordinator in V2;
- context epochs, compaction summaries, result pruning, and output spill;
- configurable built-in agents and foreground/background child tasks;
- wildcard permission rules, but explicitly no security sandbox;
- rich MCP support and very broad in-process sequential plugin hooks;
- local/remote skills, commands, references, CLI, TUI, ACP, HTTP/OpenAPI/SSE,
  PTY/WebSocket, desktop, generated clients, and an embedded internal-fetch
  SDK; and
- structured logs, OTLP, statistics, and partial performance tests.

QQ should borrow atomic event/projection semantics, durable idempotent prompt
admission, one drain per session with cross-session parallelism, typed
protocol/client composition, scoped tool registration, durable full-output
references, and catalog separation.

QQ should reject the dual V1/V2 runtime, enormous bundled dependency graph,
sequential trusted hooks, process-local background jobs, fire-and-forget plugin
readiness, optional unauthenticated server, and approval prompts without real
containment.

### Pi

Pi provides:

- a stateful streaming agent loop with text, thinking, tool events, steering,
  follow-ups, retry, cancellation, images, and reasoning controls;
- a broad provider and model API with dynamic models, SSE/WebSocket, vision,
  image generation, lazy imports, and deterministic fake providers;
- replaceable filesystem and process operations suitable for SSH, VM, or
  sandbox hosts;
- append-only branching sessions with resume, fork, clone, labels,
  export/import, and sharing;
- compaction and branch summaries;
- a broad TypeScript extension API for tools, commands, UI, providers,
  resources, hooks, themes, packages, skills, and context;
- TUI, print, JSON, RPC, embedded SDK, and an experimental strict binary
  protocol; and
- useful provider/resource/TUI telemetry and evaluation ergonomics.

Its shipped coding-agent path still uses synchronous JSONL persistence. The
newer JSONL/SQLite storage code has promising contracts, but central durable
harness operations are stubbed. MCP is an extension rather than native,
sub-agents are an example that spawns child CLI processes, extensions receive
full process authority, and listener/tool fan-out can be unbounded.

QQ should borrow provider/resource ergonomics, lazy imports and prewarming,
progressive skill disclosure, replaceable operation objects, dynamic tool
loading, differential TUI ideas, strict snapshots, and passive telemetry.

QQ should reject synchronous filesystem work in the agent loop,
publish-before-persist, full-process plugin authority by default, unbounded
`Promise.all` tool execution, and parallel legacy/stub runtimes.

### fx

fx provides:

- a small Zig runtime with streamed content/reasoning/tools, steering at model
  boundaries, cancellation, deadlines, recovery checkpoints, and explicit
  retry delivery certainty;
- three fixed provider identities with catalogs, auth, structured output,
  reasoning, vision, usage, and search;
- `glob_files`, `grep_files`, `read_file`, `write_file`, `edit_file`,
  `web_fetch`, `web_search`, `terminal`, `capability_search`, `skill`,
  `install_skill`, `subagent`, `mcp_select_tool`, `mcp_features`,
  `ask_user_question`, `vision`, and `read_tool_result`;
- excellent durable terminal semantics: exec, start, read, screen, write,
  wait, monitor, inspect, list, resize, signal, close, cursors, leases, and
  native/tmux backends;
- bounded and secret-masked tool results with opaque handles;
- strong local sessions using framed event logs, sequence and generation
  validation, checksummed replacement, periodic checkpoints, compaction,
  single-writer locking, and recovery;
- deterministic bounded compaction with stable prompt prefixes;
- rich durable sub-agent lifecycle, immutable admission snapshots,
  communication envelopes, consumer cursors, and non-escalating child
  authority;
- exact permission targets, MCP, compatible skill roots, typed compile-time
  hooks, CLI/TUI, ACP, native Node/Wasm embedding, configuration, local traces,
  deterministic replay, live-model evals, and profile-guided size tooling.

Its advertised 7.8 MiB binary and 2 ms startup budgets were not measured in
this audit. The provider, tool, and hook registries are fixed in source; no
HTTP/SSE daemon, generic provider registration, dynamic native plugin system,
fleet observability, or real OS sandbox was found. Its default maximum step
count is unbounded, and read-only parallel tools spawn an OS thread per call.

QQ should borrow delivery-certainty-aware retries, dynamic MCP schema
selection, terminal semantics, immutable sub-agent admission, exact permission
targets, performance budgets, and shared-runtime discipline.

QQ should reject fixed registries, ACP as the only service boundary,
thread-per-tool parallelism, an inert sandbox setting, unbounded default steps,
and hardening/debuggability sacrifices made only to hit a size claim.

## Hermes-Style Product Boundary

Hermes separates its platform-agnostic core from CLI, gateway, ACP, batch, and
API interfaces. Its extensions include tools, hooks, commands, memory, context,
and progressively disclosed skills. QQ should provide the lower execution
layer for that style of product rather than absorb the product layer.

References:

- [Hermes architecture](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/developer-guide/architecture.md)
- [Hermes API server](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/user-guide/features/api-server.md)
- [Hermes skill guide](https://github.com/NousResearch/hermes-agent/blob/main/website/docs/developer-guide/creating-skills.md)

The boundary is:

```text
Hermes-like product
  channels / cron / voice / web / product identity / product memory
                              |
                  qq-client / versioned HTTP
                              |
                         qq-server
                              |
                    deep SessionRuntime
                      /             \
          CompiledAgentPlan       SQLite
          - provider              authoritative events
          - tool plan                    |
          - agent pack            post-commit SSE/outbox
          - context plan
          - policy/budgets
```

A Hermes-style gateway should:

1. map its user, channel, and thread identity to a QQ `SessionId`;
2. select a versioned `AgentProfileId`;
3. submit an idempotent command containing input parts and run limits;
4. consume durable events through an SSE cursor;
5. relay approval requests or steering input;
6. feed committed events into product-owned memory or indexing; and
7. reconnect and resume without guessing whether work ran.

The preferred deployment is a QQ sidecar or private service per trusted user
or workspace boundary. A product gateway owns internet-facing authentication,
tenant identity, channel policy, and rate limiting. If a real consumer needs a
shared QQ service, scoped bearer/session tokens and resource accounting should
be designed before broad remote exposure; do not grow general IAM inside
`qq-core`.

Hermes exposes an OpenAI-compatible API. QQ may add an equivalent facade for
compatibility, but it must not become QQ's primary contract. Chat-completions
semantics cannot faithfully represent durable approvals, replay cursors,
sub-agent state, tool progress, steering, or explicit run outcomes.
