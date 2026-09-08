# Speed-First Extensible Agent Harness Backend

Status: Phases 0–4 (H0–H9) complete 2026-09-01 through 2026-09-03; compact
receipts are under "Phases 0–4 — Complete". A workspace-wide design and
performance audit on 2026-09-04 (see "Hot-Path Redesign Decisions") added
tasks H13–H22 and two new implementation phases ahead of the sandbox,
adapter, and qualification work. Phase 5 (H13–H17) complete 2026-09-04;
receipt under "Phase 5 — Correct The Hot Path", including the unmet output
service-gap gate. A follow-up source audit and peer review on 2026-09-04
identified correctness gaps in shipped child, cache, feed, and context-source
behavior. Both H23 ownership slices are implemented and locally validated on
Linux, with focused latency/resource receipts under Phase 5a; native Windows
teardown remains unqualified. H24 remaining child budgets and owned-descendant
accounting are implemented 2026-09-05; receipt under Phase 5a. H25 and H26 are
implemented and correctness-validated on Linux on 2026-09-06; their receipt
below retains the pending performance qualification. A fresh focused shell
comparison passes its noise and regression gates. The corrected fan-out
comparison and same-binary control both fail tail gates. The full version-4
H0 comparison ran on 2026-09-07 and its four original failures are resolved
by the feed hot-path redesign and release profile (receipt under Phase 5a);
remaining tail-gate failures reproduce in the same-binary control and require
a quiet-host acceptance run. Native Windows teardown passed in CI.
**Active boundary: close Phase 5a qualification. Later-phase work is saved
in isolated branches and paused until this phase is assessed.**
H23–H26 precede Phase 6; H27–H28 join its early correctness work. Phases 6–9
remain proposed, with H20 moved ahead of H18 and H19 conditional on decoder
measurements. A hosting-boundary review on 2026-09-05 added the
headless-contract tranche HC1–HC4 (Phase 5b), whose CLI and compilation work
may run in a parallel worktree alongside Phase 6. HC3 adds opt-in runtime
validation and repair before the mechanical split. Its design authority is
[`docs/design/headless-contract.md`](../design/headless-contract.md).

This plan defines how QQ becomes an extremely fast, lightweight, customizable
agent harness that can serve as the backend for products such as a
Hermes-style personal agent. It is intentionally a backend plan, not a plan to
copy every product surface from Codex, OpenCode, Pi, fx, or Hermes into QQ.

The central decision is:

> Compile customization once, execute directly in the hot path, persist before
> publishing, and keep every queue and concurrency boundary explicit.

QQ should become a small durable execution kernel with a compiled
customization plane. Messaging gateways, cron, voice, browser automation,
product identity, and user-facing memory products remain clients of that
kernel.

## Decision Summary

QQ already has the strongest combined backend foundation of the audited
projects:

- one Rust runtime shared by direct CLI, TUI, durable headless, and server
  paths;
- a provider-neutral request and streaming seam;
- authoritative SQLite events and persist-before-publish ordering;
- idempotent commands and cursor-based HTTP/SSE replay;
- bounded tools with capability-based filesystem containment;
- approval, cancellation, recovery, and cost-accounting semantics;
- MCP-based external tools; and
- durable, bounded read-only sub-agent sessions.

The compiled plan, backend protocol, and initial extension lanes have shipped.
The remaining priorities, in order, are:

1. Repair live credential binding and workspace-feed retention (H25–H26),
   and retain H23's native Windows qualification gap.
2. Finish control admission and the carried output-fairness gate (H20).
3. Repair settlement, active-plan accounting, context-source admission and
   identity, and pack revalidation (behavioral H21, H27–H28, correctness H22).
4. Reduce per-turn copying (H18), then optimize SSE only if decoder-specific
   measurements justify it (H19); leave mechanical consolidation last.
5. Add durable terminal control and a real process-sandbox adapter when
   evaluation justifies them.
6. Add optional product-facing protocol adapters only in response to real
   consumers.

Do not start with a universal plugin API. It would put discovery, dynamic
dispatch, trust, lifecycle, and failure handling in the most latency-sensitive
part of the system before QQ has measured its baseline.

## Status And Authority

This document is the active backend plan and a companion to the architecture.
It does not silently override the design documents.

- [`docs/design/architecture.md`](../design/architecture.md) remains the system
  boundary and dependency-direction source of truth.
- [`terminal-bench-readiness.md`](./terminal-bench-readiness.md) owns
  tool-contract ablations, terminal qualification, sub-agent economics, the
  remaining warm-runtime candidates, and the Terminal-Bench evaluation
  program. Its shipped phases (linear streaming, store fairness, resolved
  model, context admission, compaction, budgets) are recorded there as
  receipts.
- [`supervised-delegation.md`](./supervised-delegation.md) owns continuation
  on truncation, the delegation roster, supervised write children, the
  final-answer audit, and the pending paired evaluation.
- [`run-snapshots.md`](./run-snapshots.md) owns reversible mutating-run state.
- [`lsp-diagnostics.md`](./lsp-diagnostics.md) owns diagnostics integration and
  its MCP-first validation path.
- [`docs/design/headless-contract.md`](../design/headless-contract.md) owns
  the `qq run` JSONL/exit contract, the supervisor boundary, and the HC1–HC4
  gap list implemented as Phase 5b here.

Shipped plans (TUI rearchitecture and refinement, compaction, model-reviewed
approvals, read-only sub-agents, provider rearchitecture, client parity) were
removed on 2026-09-04; their durable content lives in `docs/design/` and
their receipts in Git history. The proposed `qq-core` physical extraction plan
was retired in favor of D9 below, and the Terminal-Bench baseline-repair
tranche folded into the readiness plan's Phase 6 gates.

Where this plan depends on one of those contracts, implementation should land
through the owning plan and this document should record the dependency rather
than duplicate the design.

The architecture currently defers a public extension interface, a plugin
marketplace, JavaScript packages, multi-user tenancy, and distributed workers.
Later phases in this plan may cross those boundaries only after their entry
gates are met and the architecture document or an ADR records the decision.
This plan is not a license to pre-build the deferred phases.

## Goals

### Product Goal

QQ should maximize verified successful agent work per dollar, per minute, and
per unit of local resource use while remaining pleasant to embed and extend.
Correctness, durability, and safe execution are baseline constraints rather
than tradeable performance features.

An application developer should be able to:

1. create or resume a durable QQ session;
2. select a versioned agent profile;
3. submit an idempotent multimodal command with explicit resource limits;
4. receive durable progress through a reconnectable event stream;
5. respond to approvals or steer an active run;
6. add tools through MCP or a trusted embedded host;
7. add instructions and skills through declarative agent packs;
8. add a provider without changing the agent loop;
9. add memory retrieval without intercepting token streaming; and
10. recover from client or process loss without repeating uncertain side
    effects.

### Performance Goal

The default path should pay only for the behavior it uses:

- configuration, discovery, trust, schema preparation, and provider selection
  happen before the run hot path;
- disabled adapter families do not add shipping dependencies to minimal
  embedders;
- active runs use immutable shared plans and direct dispatch;
- observers consume committed events asynchronously;
- queues, buffers, retries, tasks, subprocesses, and fan-out are bounded; and
- every material optimization is supported by an end-to-end measurement.

### Customization Goal

Customization should be ergonomic without collapsing unlike concerns behind
one shallow interface. QQ will expose a small set of deep extension lanes with
different trust and performance contracts:

- declarative agent packs;
- compiled provider adapters;
- static native tools;
- external or embedded tool hosts;
- bounded context sources;
- post-commit event observers; and
- surface adapters using the versioned client protocol.

## Non-Goals

This plan does not authorize:

- a universal `Plugin` trait through which every token, event, and tool call
  passes;
- a dynamic Rust shared-library ABI;
- a plugin marketplace before addon packaging and trust have two real
  consumers;
- one crate per provider, tool, storage backend, or integration;
- a second agent runtime for embedding;
- a JavaScript or Python runtime inside `qq-core`;
- messaging, cron, voice, browser automation, or product-specific memory in
  `qq-core`;
- a distributed scheduler, hosted control plane, or general multi-tenant IAM;
- parallel or unsupervised editing sub-agents before snapshots, isolation, and
  conflict semantics are implemented. One serialized `Supervised` write child
  per run, every held action adjudicated, is permitted by
  `supervised-delegation.md` (amended 2026-09-03);
- WebSocket, gRPC, GraphQL, or a binary wire protocol without measurement or a
  feature that HTTP/SSE cannot express;
- unbounded queues, tasks, listeners, retries, output, or concurrency; or
- performance claims based only on binary size, source line count, or an
  isolated microbenchmark.

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

## Target Architecture

### Compile Cold, Execute Hot

The central new deep module is an immutable `CompiledAgentPlan`:

```text
compile(profile, resolved configuration, capability snapshots)
    -> Arc<CompiledAgentPlan>

run(session, input, limits, cancellation)
    -> durable event stream
```

`CompiledAgentPlan` is a live runtime object and is never serialized. It
contains:

- the compiled provider handle and immutable model identifier;
- effective context, output, reasoning, media, cache, and pricing capabilities;
- the versioned prompt, persona, and instruction snapshot;
- an immutable tool schema and dispatch plan;
- skill, command, addon, and MCP catalog digests;
- the bounded context-source plan;
- permission, approval, terminal, and sub-agent policy;
- cost, time, turn, tool, token, output, and concurrency limits; and
- an associated secret-free `AgentPlanDescriptor` sufficient to reproduce and
  explain the execution.

`AgentPlanDescriptor` is the canonical serializable identity. It contains only
behavioral and provenance data: provider endpoint/protocol/auth scheme, the
credential reference name, model and capability values, prompt/tool/context
digests, policy, limits, addon versions, and adapter build/version identity. It
never contains a resolved key, bearer token, authorization header, secret
hash, live provider handle, callback, or process handle.

`AgentPlanDigest` is the SHA-256 of one canonical encoding of that descriptor.
The canonical encoding, field ordering, normalization, and version are contract
fixtures. Secret values are neither hashed nor persisted. Credential rotation
uses a separate opaque, non-secret `CredentialEpoch` owned by `qq-auth`.
Rotation invalidates or reauthorizes matching live cache entries, and accepted
runs may record the opaque epoch for diagnosis without changing the behavioral
plan digest or revealing credential material.

Provider names, addon names, config layering, manifest discovery, schema
compilation, filesystem discovery, secret resolution, and trust decisions do
not belong in the turn loop. The loop receives direct handles and exhaustive
provider-neutral values.

Plans are cached by the plan digest plus adapter build identity and credential
epoch. A config, skill, tool schema, profile, provider, policy, model, or
adapter change produces a new generation. Active runs retain their original
`Arc`; a successful refresh atomically publishes the new generation for later
runs. A failed refresh does not poison an already-valid plan.

The cache has hard entry-count and estimated-byte ceilings established from
Phase 0 measurements. It evicts least-recently-used inactive generations.
Active `Arc` generations count toward the memory ceiling but cannot be evicted;
if admission cannot fit another plan after inactive eviction, compilation fails
with an explicit capacity error rather than growing without bound. Tests cover
eviction, active-generation pinning, refresh storms, credential rotation, and
shutdown.

### Ownership Within The Existing Crates

Do not add an agent-framework, plugin, tool-host, context, or addon crate.

| Owner | Responsibility |
| --- | --- |
| `qq-config` | Parse and merge agent-profile and addon declarations, validate syntax, retain provenance |
| `qq-auth` | Resolve provider and addon secret references without exposing secret values to config or protocol |
| Root package | Discover trusted sources, translate external config, compile/cache plans, and wire concrete adapters |
| `qq-provider` | Compile provider recipes and expose one provider-neutral stream handle plus effective capabilities |
| `qq-core` | Own `CompiledAgentPlan`, secret-free `AgentPlanDescriptor`, execution invariants, context-source contracts, tools, run limits, and durable outcomes |
| `qq-mcp` | Supply bounded MCP capability/catalog snapshots and execute selected MCP operations |
| `qq-protocol` | Own versioned profile IDs, plan digests, input parts, commands, outcomes, events, and capabilities |
| `qq-server` | Map authenticated HTTP/SSE requests onto protocol commands; no agent logic |
| `qq-client` | Provide bounded command, reconnect, replay, approval, steering, and capability APIs |
| `qq-tui` | Project protocol state; never discover or execute addons itself |

Application configuration types must not leak into `qq-core`. The root
translates them into typed provider handles, tool/catalog snapshots, context
plans, and policies.

### Extension Lanes

| Lane | Interface | Load time | Hot-path behavior | Trust/isolation |
| --- | --- | --- | --- | --- |
| Agent packs | Declarative versioned manifest | Discovery/startup | Immutable prompt/profile data | Hash and trust source; no code execution |
| Providers | Existing Rust provider compiler/stream seam | Startup or explicit refresh | Direct provider handle | Compile-time trusted adapter; secrets resolved outside core |
| Native tools | Static Rust registration | Build/startup | Direct dispatch | Fully trusted; capability-scoped execution |
| General tools | MCP, then a real embedded callback host | Startup catalog; call on demand | One selected adapter call | MCP process/HTTP boundary or trusted embedder |
| Context/memory | Typed bounded `ContextSource` | Plan compile plus pre-turn fetch | No per-delta hook | Time/byte/token budgets; fail policy explicit |
| Observers | Durable SSE/outbox | Subscription | Post-commit only | Cannot affect authoritative execution |
| Process execution | Local implementation plus one real sandbox adapter | Startup | Direct selected backend | Explicit filesystem/network/process capabilities |
| Surface adapters | Versioned `qq-client` contract | Client startup | Outside agent loop | Product owns remote auth and UX |

An addon package may declaratively bundle profiles, prompts, skills, commands,
MCP servers, context-source declarations, configuration schemas, and secret
references. The manifest is packaging and provenance, not a universal runtime
plugin object.

### Agent Packs

An agent pack is the cheapest and safest customization path. Its versioned
manifest should eventually support:

- stable `id`, version, display name, and schema version;
- one or more agent profiles;
- prompt/persona/instruction resources;
- skill and command roots;
- model and provider constraints;
- tool allow/deny/exposure policy;
- MCP server references;
- context-source declarations;
- run and sub-agent budget defaults;
- required configuration keys and secret references;
- minimum QQ protocol/capability requirements; and
- content digests for every execution-affecting resource.

Loading must use progressive disclosure. Pack metadata is cheap and available
at discovery; full skill or prompt content enters a plan only when the selected
profile requires it. A pack may not contain arbitrary native executable code.
Executable behavior arrives through an already-trusted native tool, MCP, or a
future embedded host.

### Tool Hosts

The built-in Rust tools remain the zero-overhead path. Do not wrap them in RPC
or a dynamic plugin abstraction.

MCP remains the general external executable-tool path. Improve it with dynamic
capability search and selected-schema loading so a large remote catalog does
not inflate every model request.

Only when an actual embedding consumer needs application callbacks should QQ
add a second external-tool adapter. At that point deepen MCP and embedded
callbacks behind one `ExternalToolHost` contract that provides:

- immutable catalog generation and digest;
- tool schema, annotations, effect class, and ownership;
- bounded request and result sizes;
- deadline and cancellation;
- concurrency and queue limits;
- readiness and shutdown;
- exact error and retry semantics; and
- optional durable full-output references.

The hot path selects one precompiled tool entry. It does not execute a list of
before/after plugin hooks or rediscover schemas.

### Context And Memory

Product memory is not a synchronous observer of every token. A
`ContextSource` supplies bounded pre-turn context and may consume committed
events asynchronously after the fact.

The source contract needs:

- stable identity and version;
- deterministic cache key inputs;
- a time, byte, result, and estimated-token budget;
- cancellation;
- provenance attached to inserted context;
- an explicit fail-open or fail-closed policy; and
- no authority to modify durable transcript history.

Ordinary retrieval should fail open with a visible diagnostic rather than
block an otherwise valid run. A product that requires a memory checkpoint
before compaction may opt into a specific fail-closed pre-compaction operation;
that is a separate contract from ordinary retrieval.

Post-commit event ingestion should use the same durable cursor semantics as
other clients. Memory consumers may fall behind and replay; they cannot delay
the event commit or user-visible stream.

### Observers And Hooks

Use durable event subscribers for logging, analytics, memory ingestion,
notifications, and product automation.

Synchronous decisions remain limited to core invariants such as approval,
exact tool validation, and budget admission. If an external policy engine is
eventually required, it must have a typed request, a strict deadline, a
documented fail policy, and no access to arbitrary stream mutation.

Do not provide arbitrary synchronous pre/post hooks around provider deltas,
tool output, persistence, compaction, or session lifecycle. Codex, OpenCode,
and Pi demonstrate how ergonomic broad hooks can silently add tail latency,
failure coupling, and unbounded authority.

### Provider Footprint

Keep the current provider crate and public `Provider::stream` seam. Feature-gate
adapter families internally. The H1 audit settled on one feature:

```text
provider-bedrock   (default; owns the seven optional AWS SDK crates)
test-support       (loopback fixtures; not a public API)
```

The HTTP families need no feature because they add no dependency beyond the
shared reqwest transport, so a `provider-http` flag would gate nothing. The
shipped full `qq` binary enables all supported providers, while embedders build
`--no-default-features` and compile only the HTTP families. Neutral request, model, pricing, event, and compiler types
remain available without pulling every vendor SDK.

Do not introduce one-provider-per-crate organization. Wire-format differences
are real, but the existing crate already centralizes shared HTTP, SSE,
redaction, construction, and compilation behavior. Split only measured heavy
dependencies, not cohesive protocol code.

## Hot-Path Redesign Decisions

On 2026-09-04 seven read-only audits covered every crate for design-pattern
and performance opportunities, and one selection pass verified the
highest-stakes claims against source and chose the designs below. The
selection criteria, in order, were: measurable movement on a named gate,
correctness, line reduction with no behavior change, and simplicity (enums,
tables, newtypes, and const data over traits; one owner over layers; no
dynamic dispatch on hot paths; no new crates). Each design names the
pattern it applies honestly; several are "a struct" or "a table", which is
the point.

### Findings At The Start Of Phase 5

This table records the pre-change audit, not the current implementation.
H13–H17 receipts below identify repairs and the remaining acceptance gap.

| Finding | Evidence | Verdict |
| --- | --- | --- |
| One fsync per output batch | `sessions/store/worker.rs` runs one job per message with no outer transaction; `append_text`, `append_reasoning`, `append_tool_call_output`, and `append_run_activity` each open and commit their own transaction under `synchronous=FULL`; zero `prepare_cached` in `qq-core` | Confirmed |
| Every subscriber re-reads SQLite per event | `sessions/runtime.rs` subscribers call `store.events_after` after every wakeup; `notify` publishes only a sequence; `read_events` parses each row; the server re-serializes each envelope per subscriber | Confirmed |
| Two retry owners | `qq-provider/http.rs` retries three attempts within 15 s; `qq-core/runtime/retry.rs` retries eight attempts from 1 s to 60 s around it; worst case 24 sends per logical turn. The provider's `random_u32` fallback yields zero jitter | Confirmed |
| `ext__` tools bypass approval | `approval.rs` classifies only `mcp__`; every other unknown name maps to `Unknown`, which executes in every mode including read-only; `hosts.rs` documents the opposite | Confirmed |
| History cloned per attempt | `lib.rs` builds `ModelRequest::new(.., messages.clone(), ..)` inside the attempt loop; `ModelRequest.messages` is an owned `Vec<Message>`; adapters use only `&[Message]` | Confirmed |
| `SessionEvent` size dominated by inline `SessionSummary` | Eight variants embed the 18-field summary by value; `TextAppended` carries one string | Structure confirmed; byte figures not measured |
| Active-run summary scans events on the ack path | `load_session_summary` calls `load_run_activity`, which parses up to 512 envelopes inside the command transaction | Confirmed |
| Config parsed three times per load | `load`, `selected_organization`, and `project_trusted` each discover, read, parse, and hash the same project files | Confirmed by audit |

### D1 — Published-Event Outbox And Workspace Broadcast

Pattern: Observer through `tokio::sync::broadcast`; the outbox is a value
returned from the commit path.

Problem: each committed event is re-read from SQLite and re-parsed by every
subscriber and re-serialized by the server; `notify` is called from 31
ad-hoc sites with only a cursor. Subscriber count multiplies control-lane
store load and competes with command acknowledgement.

Design, owned by `qq-core::sessions`:

```rust
pub struct PublishedEvent { pub envelope: SessionEventEnvelope, pub json: Arc<str> }
pub(super) struct Committed<T> { pub value: T, pub events: Vec<Arc<PublishedEvent>> }
struct WorkspaceFeed { tx: broadcast::Sender<Arc<PublishedEvent>> } // bounded, 1024
```

`append_event` already encodes the envelope before the insert; it returns the
encoding instead of dropping it. Store wrappers return `Committed<T>`; the
runtime publishes once per commit. `subscribe` catches up from SQLite through
a raw `(sequence, String)` read until it reaches the first buffered sequence,
then drains the broadcast; `Lagged` falls back to SQLite catch-up. The server
writes `json` directly into the SSE frame; the TUI and headless adapter use
`envelope`. The sequence `watch` remains only for shutdown and failure.

Rejected: per-subscriber `mpsc` fan-out (per-client backpressure with no
bounded catch-up story); caching parsed pages (still one store read per
subscriber per event).

Gates: durable delta to TUI p95, 100-session throughput, command
acknowledgement tail. Expected: one serialization per event instead of one
plus one per subscriber; zero store reads per event in steady state. No
protocol or schema bump; wire bytes are identical.

Tests: ordering and no-gap across the catch-up to live handoff; `Lagged`
replay without duplication; eight subscribers perform exactly one catch-up
read each. Benchmark before: subscriber fan-out at one, eight, and
thirty-two subscribers on the existing SSE observation harness.

### D2 — Output-Lane Group Commit And Statement Cache

Pattern: unit of work in the store worker with deferred replies.

Problem: every output job commits and fsyncs its own transaction, so eight
streams cost eight fsyncs per service round and the 50 ms starvation budget
sits at 49–50 ms. No statement is cached. `append_event` runs an `UPDATE`
then a `SELECT`.

Design, owned by `qq-core::sessions::store::worker`: when an output job is
dequeued, the worker opens one transaction, runs that job and any already
queued output jobs (bounded by `OUTPUT_GROUP_LIMIT = 16` and by control-lane
emptiness) each inside a savepoint, commits once, and only then replies to
every job with the shared commit outcome. A failing job rolls back its own
savepoint and receives `Persistence`; a failing outer commit fails every job
and publishes nothing. Control-lane jobs keep their own transaction so an
acknowledgement never waits for a batch. The connection sets a prepared
statement cache of 128 and hot statements use `prepare_cached`; `append_event`
uses `UPDATE … RETURNING next_sequence`.

Rejected: `synchronous=NORMAL` (a failed write could be presented as
durable); a timer-based batch window (adds latency to a lone stream, while
draining already-queued work adds none).

Gates: eight-stream output service gap (expect ≤20 ms from ≤50 ms), semantic
delta to durable commit p95 and p99, one MiB scaling. Persist-before-publish
is preserved because replies, and therefore D1 publication, fire only after
the outer commit.

Tests: savepoint isolation; commit failure fails every job and publishes
nothing; a control job is admitted between batches; a crash after savepoint
and before commit leaves nothing durable. Benchmark before: a
`store_output_batch` bench (eight streams, 64-byte deltas) plus the R4
fairness matrix.

### D3 — Single Retry Owner

Pattern: remove a layer; one policy struct on the compiled provider.

Problem: the core turn loop retries the provider's retries. Up to 24 sends
per logical turn violate the amplification gate and the architecture rule
that retry belongs to `qq-provider`. Core also re-sends when a stream ends
without a terminal event, a decision the provider can make with better
information.

Design: `qq_provider::RetryPolicy` becomes a public, plan-compiled
`AttemptPolicy` (default four attempts, 500 ms base, 8 s cap, 30 s budget)
carried on the compiled provider. `Provider::stream` restarts the request
when the SSE stream ends before any `ProviderEvent` has been yielded, where
duplication is impossible, and counts it against the same policy.
`ProviderError` records the attempt count. `qq-core/runtime/retry.rs`,
`TurnRetryPolicy`, and the `'turn` attempt loop are deleted. The `random_u32`
fallback becomes an atomic generator seeded from `SystemTime`.

Rejected: keeping core as owner with provider retries disabled (core lacks
`Retry-After` and pre-stream versus post-stream visibility); splitting
ownership by error kind (two owners is the bug).

Gates: provider retry amplification below 1.05; claim to provider send (no
per-attempt request rebuild).

Tests: fake provider failing before the stream and mid-stream before the
first event; attempts never exceed the policy; zero duplicate deltas;
`Retry-After` honored; attempts visible in the failure outcome. Add an
amplification counter to the fake-provider benchmark.

### D4 — Effect-Classified Approval

Pattern: table lookup on data the catalog already holds.

Problem: `approval::classify` keys on the tool name; embedded-host `ext__`
tools fall to `Unknown` and execute in every mode. Effect is re-derived from
name strings in four places.

Design: `RuntimeToolCall` carries the catalog `EffectClass`, set at lookup.
`classify(effect, name, args)` matches on the effect and consults arguments
only for the shell and `spawn_agent` refinements it performs today. External
tools map to the class MCP tools use now. `ToolClass::Unknown` is deleted; a
name absent from the catalog is a tool error before approval. The name
matches in `tools/specs.rs`, `plan.rs`, and `approval.rs` are removed. Host
`read_only` hints stay advisory.

Rejected: adding an `ext__` prefix arm (fixes the symptom and keeps four
derivations); letting hosts declare effect (hints must not grant authority).

Gates: none directly; removes one JSON parse and two string matches per tool
call. This is a P0 correctness fix. No wire, schema, or descriptor change,
because effect is derived from the already-digested catalog.

Tests: `ext__` tool denied under read-only, held under ask and supervised;
hints do not change decisions; MCP behavior unchanged.

### D5 — Shared Transcript And Precompiled Prompt Prefix

Pattern: shared immutable data with copy-on-write; memoization in the plan.

Problem: the history is deep-cloned per turn; the system prompt (up to
~128 KiB) is rebuilt and hashed per run; tool schemas are re-serialized per
run although the catalog serialized them at compile; message bytes are
measured twice per run.

Design: `ModelRequest.messages: Arc<Vec<Message>>`; core appends with
`Arc::make_mut`, which does not copy once the previous stream is dropped.
`CompiledAgentPlan` gains `prompt_prefix: Arc<str>`, a cloned SHA-256 state
for the prefix that is fed only the per-run suffix, and the
`ToolSchemaMeasurement` for full and static exposure computed once.
`ToolSpec.input_schema` becomes a precomputed `RawValue`; tool-call
arguments in history are stored as their original string rather than
re-stringified per request. Core keeps running byte counters updated on push.

Rejected: `Arc<[Message]>` (cannot push; copies per turn); a persistent
vector crate for one site.

Gates: one MiB request heap ≤2x (from ~3x), encode ≤10 ms, claim to provider
send (−100–300 µs prompt rebuild, −0.5–1 ms schema measurement).

Tests: `Arc::strong_count == 1` after stream drop proves no copy; the
prefix-plus-suffix digest equals the full digest, which guards the persisted
`RunPromptIdentity`. Benchmark before: `provider_encode` (one MiB plus 32
schemas with a counting allocator) in `qq-provider`; rerun
`provider_compiler` and `plan_compile`.

### D6 — Command-Acknowledgement Fast Path

Pattern: denormalized column and a maintained counter.

Problem: `load_session_summary` runs on every command that publishes a
summary; for a session with an active run it parses up to 512 envelopes
inside the command transaction (2–20 ms). Every command counts the
`commands` table. Snapshots scan accounting per session.

Design: schema 24 adds `runs.activity`, written by `append_run_activity` in
the same transaction as its event and read by the summary query;
`load_run_activity` is deleted. The command count becomes a maintained
counter or an existence check where only presence matters. `load_snapshot`
aggregates accounting in one grouped query. Migration backfills `activity`
as null, which clients already tolerate.

Rejected: an in-memory activity cache (a second source of truth that must
survive restart).

Gates: command acknowledgement ≤10 ms with an active run; snapshot and
reconnect latency. Schema bump (shipped as 24→25: the audit columns took 24 first); no wire bump.

### D7 — Two-Hop Claim To Send

Pattern: query consolidation.

Problem: a claim performs five serialized store round trips before the
provider send; context assembly is N+1 per message and parses every
tool-call argument; `canonicalize` runs inside a store closure on the worker
thread.

Design: one control job returns the claimed run, file state, pending
steering, cancellation flag, and messages from a single transaction using
one joined query over messages, chunks, and tool calls ordered by ordinal.
Stale-result pruning uses stored kinds rather than re-parsed arguments.
`start_reserved_run` remains the second hop because it publishes
`RunStarted`. Path canonicalization moves ahead of the queue on the caller's
blocking thread.

Rejected: caching assembled context across turns in memory (invalidated by
steering and compaction; adds an owner).

Gates: warm claim to provider send ≤25 ms. Shares the schema 24 migration
with D6.

Tests: a context-equality fixture comparing the old assembly with the
joined query on a store seeded with compaction, steering, and pruned
results; the reload path uses the same function.

### D8 — Wake-Driven Control Admission

Pattern: bounded semaphore replaces polling.

Problem: fourteen sites loop on `sleep(1 ms)` after `Overloaded` from the
control lane, adding jitter on cancellation and settlement; sub-agent
completion polls the store on every workspace event; MCP cancellation polls
every 50 ms.

Design: the store gains `control_slots`, a semaphore mirroring the existing
`output_slots`; control calls await a permit so `try_send` cannot be full.
`Overloaded` remains only for explicit admission rejection. The fourteen
loops are deleted. Sub-agent completion subscribes to the existing
`settlements` watch; MCP cancellation uses a watch.

H23 introduces the shared `control_slots` capacity bound and uses waiting
admission for child outcome reads and owned-child cancellation. H20 extends
that mechanism to the remaining lifecycle callers and removes the existing
polling loops; H23 does not qualify those broader changes or the fairness gate.

Gates: cancellation ≤100 ms under load; command acknowledgement tail; the
carried eight-stream output service gap ≤20 ms. Queue admission, dequeue,
and commit timing in a focused fixture must establish the remaining cause;
persisted event timestamps alone do not identify scheduler wake latency.

Tests: 256 concurrent control calls complete without spinning;
cancellation latency under load.

### D9 — Store Identity, Settlement, And Error Consolidation

Pattern: `Copy` newtype, constructor functions, one settlement value, and
`From` impls. No traits.

Problem: `ClaimedRun` is cloned 23 times and fabricated six times;
`EventContext` is written as a literal 46 times; three settlement paths
carry divergent guards (`complete_run_in_transaction` lacks the
`outcome_json IS NULL` guard, a latent double settle); 332
`map_err(|_| Persistence)` sites erase every SQLite error.

Design: `RunIdentity { workspace_id, session_id, run_id, command_id, kind,
child }` is `Copy` and lives inside `ClaimedRun`; `EventContext::for_run`
and `for_session` replace the literals; `RunSettlement { identity, outcome,
audit }` feeds one `settle_run` with the null guard; `PersistenceFault
{ Sqlite(code), Codec, Constraint }` rides in
`SessionRuntimeError::Persistence` through `From<rusqlite::Error>`. After
that lands, `sessions.rs` is split into `sessions/{codec, events, snapshots,
transcript, claim, streaming, tool_calls, settlement, compaction,
commands}.rs` with tests under `sessions/tests/`, as a separate mechanical
commit. The settlement interface must also make successful execution teardown
a prerequisite for publishing a terminal event or releasing session ownership.
H23 fixes the existing branches explicitly; H21 makes that ordering structural
so a new branch cannot silently bypass child and local-tool drain.

Gates: none directly (about five fewer allocations per persisted event).
This is correctness plus roughly 700 fewer implementation lines. The HTTP
mapping of `Persistence` is unchanged.

Tests: settling an already-settled run through the previously unguarded
path is a no-op; every `PersistenceFault` variant is reachable through fault
injection.

### D10 — Zero-Copy SSE Framing

Pattern: slice scanning with a borrowed view.

Problem: the provider SSE decoder pushes byte by byte, allocates name and
data strings per event, Anthropic parses each event twice, and the ledger
clones ids per argument delta; the client decoder mirrors the per-byte
feed.

Design: `SseFramer::push(&[u8])` scans for frame boundaries with a slice
search and yields `SseEventRef<'a> { name, data, id }` over the framer's
buffer; adapters parse `data` once; `ProviderEvent` tool ids become
`Arc<str>`. The client reuses the same shape; the framer is duplicated
rather than shared if sharing would add a dependency edge.

Rejected: an event-source crate (a dependency for bounded behavior QQ owns);
parsing to `RawValue` then re-parsing (still two passes).

Gates: decoder time and allocations on a deterministic local HTTP/SSE
pipeline, plus semantic delta to durable commit. The existing one MiB to
512 KiB ratio remains a regression gate, but its fake provider emits semantic
events directly and does not exercise either SSE decoder. Record the parser
baseline before committing to the borrowed-view design and retain it only if
the measured benefit justifies the interface change.

Tests: property test splitting events at every byte boundary; CRLF;
multi-line data; oversized rejection. Benchmark before: `sse_decode` at
64 KiB, 512 KiB, and one MiB in `qq-provider`, which does not exist today.

### Bundled Fixes (H22)

Small enough to ship alongside the designs, grouped by crate:

- `qq-core`: move `catalog_blocking` inside `spawn_blocking`; hoist
  `sleep_until` out of three `select!` loops; take tool results by value;
  parse tool arguments once into `RuntimeToolCall`; `TurnMode` and
  `StreamEnd` enums; one bounded UTF-8 read for six copies; serialize the
  descriptor once at compile; gate file-state eviction on a counter; borrow
  when persisting model turns.
- `qq-mcp`: share catalog `ToolSpec`s by `Arc`; release the call permit
  before awaiting the connect mutex.
- `qq-protocol`: box `SessionSummary` in the eight summary-carrying event
  variants (wire-neutral); one hash newtype macro for the two identical
  32-byte hash types; move client body limits into `limits.rs`.
- `qq-provider`: one `StaticHttpAuth` with a per-protocol API-key header
  constant, deleting three auth enums and four `build_headers`
  (−240 lines); `RequestAuthorizer` as an enum; body encoding with a
  capacity hint; skip redaction merge when empty; avoid the per-request
  `HeaderMap` clone; stop `Debug`-formatting Bedrock events to count bytes.
- `qq-server` and root: a `COMMAND_ROUTES` table keyed by
  `SessionCommandKind` replacing twelve handlers, with the client table
  asserted equal in a test; `decode_bounded` for the four decode preambles;
  `PlanCache` borrows the key on lookup, evicts single-flight entries, and
  stops cloning paths in fingerprint checks (active-generation accounting and
  atomic refresh admission are separately tracked by H27); the approval reviewer compiles
  through `PlanCache`; headless output leaves the Tokio worker.
- `qq-config` and `qq-auth`: explicit pack manifests enter `probed_paths`
  (warm revalidation misses them today); pack MCP servers pass
  `validate_mcp_servers`; parse each source once per load instead of three
  times; memoize verified ancestor directories instead of re-stat'ing from
  root; `LazyLock` builtin providers; shared lock for credential reads,
  `chmod` only when the mode differs, and `resolve_with_epoch` to halve lock
  cycles.
- `qq-tui`: a `body_mut` that does not drop the session tree index on
  streaming deltas; scan the streaming tail once per frame; compute sidebar
  status only for visible rows; bound `tool_timing` and
  `expanded_tool_calls`; parse key chords once.

### Rejected Or Deferred

- A `SseCodec` trait with a generic `SseProvider<C>` over four stable
  adapters: trait-shaped, no gate, broad regression surface. Revisit when a
  fifth SSE adapter forces it.
- Bedrock and Google stream phase enums and the duplicate tool-call
  tracker: correct but no gate; fold into the next adapter change.
- Collapsing boxed `stream!` layers: one indirection per event, not per
  byte; no measurement.
- Template-method tool-call transitions and per-arm command functions in
  `execute_command`: shortening for its own sake; reconsider only if the D9
  split exposes real duplication.
- Typed run and tool-call state strings, `RunShared`/`RunState` bundles,
  table-driven budget checks, search/read/hex micro-optimizations: no gate
  or owned by R6 evaluation.
- A shared `AnswerProjection` across the TUI, headless, and `ask` reducers:
  speculative unification; the TUI reducer is cached and correct.
- Root canonicalize and credential-plan dedup, `Tracked<T>` provenance, a
  `RefreshableCredential` trait across Codex and xAI: cold path, no gate.
- TUI reducer and view dedups beyond H22: frame cost at 200 sessions is
  30–60 µs.
- File splits other than `sessions.rs`: structure without behavior; do
  opportunistically, never as a dedicated task.
- A `ValidatedInput` newtype: no bug reported.

### Follow-Up Correctness Audit — 2026-09-04

Three independent read-only reviews of `036329a` covered performance,
architecture, and correctness, then challenged each other's recommendations.
The resulting work repairs existing contracts before further optimization:

| Task | Current discrepancy | Required repair and acceptance |
| --- | --- | --- |
| H23 | A child outcome-read error disarms cleanup and releases its writer permit while the child may still execute. Interrupting steering can drop admission before a child id is returned or abandon asynchronous cancellation while the parent continues. | Preserve ownership across admission, overload, interruption, and cleanup. A parent or replacement writer cannot mutate until the previous supervised child has stopped. Test held creation, control saturation, interrupting steering, failed cleanup, and durable terminal ordering through `SessionRuntime`. Ordinary parent cancellation already has a creation-race regression; steering needs its own. Owned by supervised-delegation D4. |
| H24 | Remaining child limits are captured once per tool turn and reused after earlier sequential children consume them. | Compute remaining cost, tokens, and duration at each sequential admission; reject an exhausted remainder. Test two children in one turn, including unknown spend. Record parallel-child reservation or permitted overshoot semantics explicitly without introducing a general scheduler. Owned by supervised-delegation D2. |
| H25 | Same-file inline provider-secret changes can be discarded as equivalent plans. MCP registry keys also collapse differing inline bearer values before secrets are resolved. | Separate secret-free durable identity from live binding invalidation in both caches. Test same-path provider key/header rotation and two workspaces with differing inline MCP bearers; preserve active-run handles. Stored-credential rotation already works. Never hash or persist secrets to repair the key. |
| H26 | Subscription allocates a retained workspace feed before validating workspace existence; rejected/disconnected arbitrary ids leave entries behind. | Validate before retaining feed state and bound or reclaim inactive entries, preserving attach-before-catch-up ordering. Test unknown-id churn, disconnect, and replay/live handoff. A standalone probe of unchanged `feed.rs` retained about 129 MiB after 4096 distinct subscribe/drop pairs with no surviving receivers; authenticated HTTP reachability was source-traced, not benchmarked. |
| H27 | Same-key refresh removes the old generation before admission; active holders disappear from estimated-byte accounting, and a rejected replacement loses the old cache entry. Equivalent-plan refresh can enlarge source evidence without admission; completed per-key compile guards remain retained. The request key also derives hashing/debug output over explicit configuration contents. | Account for superseded live generations and admit atomically, including equivalent-plan evidence growth and retained request keys. Reclaim completed per-key guards without allowing overlapping same-key compiles. Use private exact request equality with redacted diagnostics and no secret-content hashing. A probe with one entry and a 17682-byte budget retained two generations totaling 23651 estimated bytes. Test pinned same-key refresh, release/reclamation, failed replacement preserving the prior generation, source-evidence growth, and distinct-key churn. Runtime concurrency independently bounds active execution; it does not reclaim completed guards. |
| H28 | A ninth context source, including a fail-closed source, is silently ignored. Source identity, version, budget, and failure policy are absent from the plan descriptor. | Reject excess registration with a typed capacity error before provider work; include immutable source descriptors in canonical plan identity with the required descriptor-version fixture update. Public-API probes reproduced both a skipped required source and equal digests for differing source identities/policies. |

The review retained the single runtime, provider-owned retries, immutable plans,
and persist-before-publish ordering. It ranked child ownership ahead of feed
retention, classified cache accounting as P2 because other runtime limits
exist, and made H19 conditional on measurements that actually decode SSE.
Fault, cancellation, and recovery fixtures land with each repair; H12 later
requalifies the combined system rather than being the first failure test.

## Backend Protocol Additions

The native QQ protocol remains the authoritative interface. Additions are
versioned and transport-neutral.

### Structured Input

Replace the external assumption that a prompt is only a string with bounded
input parts such as:

```text
InputPart
  Text
  WorkspaceFileReference
  ImageReference
```

The first implementation should not create a general asset service. Workspace
references remain capability-scoped and optionally hash-bound. Image content
may use a bounded request representation or an opaque server-owned content
reference. Provider capability validation occurs before accepting a run that
cannot consume the parts.

### Agent Profiles And Plan Identity

Add:

- `AgentProfileId`;
- selected profile on session creation or explicit session update;
- effective `AgentPlanDigest` on accepted runs and snapshots;
- source/addon/profile versions in trace metadata; and
- capability errors when a profile cannot compile in the current runtime.

The persisted run records the secret-free `AgentPlanDescriptor`, its digest,
and the opaque credential epoch—not the live compiled plan or secret material.
A later configuration or credential refresh must not change an accepted run's
identity or live handles.

### Core-Owned Run Limits

Define one provider-neutral `RunLimits` contract covering the supported subset
of:

- elapsed time;
- provider turns;
- total and per-slice tool calls;
- input and output tokens where measurable;
- estimated cost;
- tool/output bytes;
- child count, depth, and concurrent children; and
- provider/tool concurrency.

Every accepted limit yields a typed core-owned terminal outcome. A caller must
not believe a bound was enforced when the active provider cannot supply the
required accounting signal.

### Steering And Interruption

Distinguish:

- queueing the next user prompt;
- steering an active run at a safe model/tool boundary;
- interrupting the current provider/tool operation; and
- cancelling the run to a durable terminal outcome.

Each command is idempotent and receives an observable accepted, rejected,
superseded, or terminal response. Steering may not mutate a provider request
already known to be possibly delivered.

### Capability Discovery

Clients need a versioned capability document covering:

- protocol version;
- supported input parts;
- provider/model capabilities;
- available agent profiles;
- approval and steering operations;
- tool-host and context-source health;
- maximum request/event sizes;
- server feature generations; and
- optional compatibility facades.

Clients format supported behavior; they do not infer it from provider names or
silently fall back.

### Correlation Metadata

Allow a small bounded opaque correlation map on session/run creation for a
gateway's user, channel, thread, request, or job identifiers. QQ stores and
returns it for attribution but does not interpret product identity or use it
as authorization.

## Performance Constitution

### What To Measure

H0's Phase 0 deliverables are the canonical scope for the initial baseline.
They measure only current, publicly observable default behavior:

- release binary size and dependency closure;
- first and repeated fresh-process command startup, without claiming control of
  the operating-system page cache;
- server readiness plus new-store, existing-store, and idle-shutdown latency;
- idle release-server RSS and isolated active load-worker RSS;
- direct and HTTP command acknowledgement;
- submit start to deterministic provider entry as a public upper-bound proxy
  for scheduler admission and provider handoff;
- provider semantic delta to post-commit core observation;
- provider semantic delta to authenticated HTTP/SSE client observation;
- existing read and one-shot shell tool dispatch through durable completion;
- cancellation and shutdown;
- snapshot, reconnect, and replay;
- one, ten, and one hundred concurrent sessions; and
- long-stream scaling at 64 KiB, 512 KiB, and one MiB.

The SQLite commit instant and exact provider-claim/send instant are not exposed
by current public seams, so Phase 0 labels those observations as proxies rather
than adding generic runtime instrumentation. Exact commit-to-TUI rendering is
qualified by its owning readiness work. R4's complete streaming/fairness matrix
is now qualified and imported by the Phase 1 receipt below. Compaction/context
planning and sub-agent fan-out, fairness, cost, and memory remain owned by R5
and R7 and are imported only when those milestones complete. Completing H0 did
not itself complete any readiness milestone.

Use fake providers and temporary stores for deterministic runtime latency.
Separate provider network latency from QQ latency. Use fixed-model live runs
only for outcome and cache qualification.

Do not benchmark nonexistent behavior in Phase 0. H1 adds and then measures
full/minimal feature profiles. H2 adds a plan-compilation/cache benchmark. The
terminal-owning readiness phase adds persistent-terminal benchmarks if that
contract ships. Every later phase records the pre-change baseline for its own
new behavior before enforcing a regression gate.

### Existing Performance Targets

Carry forward the active readiness targets:

| Gate | Target |
| --- | ---: |
| Command acknowledgement p95 | `<= 10 ms` |
| Warm claimed run to provider send p95 | `<= 25 ms` |
| Semantic delta to durable commit | `<= 15 ms` p95; `<= 40 ms` p99 |
| Durable delta to TUI | `<= 25 ms` p95; `<= 60 ms` p99 |
| Cancellation | `<= 100 ms` |
| Output starvation with eight active streams | None longer than `50 ms` |
| One MiB request plus 32 schemas | `<= 10 ms` encode; heap `<= 2x` payload |
| One MiB stream scaling | `<= 2.2x` the half-size work after fixed cost |
| Context overflow sent to a provider | Zero |
| Compaction reduction when required | At least `8x` |
| Stable-prefix provider cache use | At least `80%` where supported |
| Core retry amplification | `< 1.05` provider stream entries per logical turn; transport attempts separately obey `AttemptPolicy` |
| Harness-attributable evaluation failures | `< 0.5%` |

Phase 0 establishes honest binary-size, startup, and RSS baselines. Its first
versioned budget carries forward directly enforceable readiness targets and
adds conservative relative regression limits; a current baseline may therefore
be correctly red without making the measurement harness incomplete. The fx
7.8 MiB and 2 ms claims are useful ambition, not transferable QQ targets: QQ
intentionally ships SQLite, HTTP/SSE, multiple provider protocols,
authentication, and stronger durability.

### Extension Performance Invariants

- A disabled adapter family adds no shipping dependency to a minimal build.
- Disabled addons add no run-loop allocation and no observer dispatch.
- Plan lookup is digest/cache lookup, not filesystem discovery.
- Tool/provider selection is resolved before repeated model turns where
  possible.
- No observer can block durable commit or client delivery.
- Every extension queue and concurrency permit is bounded.
- A new extension mechanism may not regress the disabled/default hot path by
  more than five percent without an explicitly accepted tradeoff.
- Catalog changes compile a new immutable generation rather than mutating a
  live registry under the run loop.
- Active runs never wait for an unrelated addon reload.
- All runtime traces identify the exact plan and addon generations.

## Keep, Redesign, And Reject

### Keep

- The current crate graph and root composition boundary.
- One runtime shared by all execution surfaces.
- `ProviderCompiler` and the small provider-neutral `Provider::stream` seam.
- SQLite as the first authoritative store.
- Persist-before-publish and one terminal event per accepted run.
- Idempotent commands, cursor replay, and HTTP/SSE.
- Static built-in tools as the fastest execution path.
- MCP as the general executable-addon protocol.
- Capability-scoped filesystem access, exact approval targets, CAS writes,
  bounded output, and cancellation.
- Durable child ownership, recovery, limits, and accounting.

### Redesign First

| Area | Problem | Direction |
| --- | --- | --- |
| Streaming store | Full reconstruction and string concatenation | Linear chunks and materialization at read/compaction boundaries |
| Reasoning store | Transaction per delta | Bounded batches preserving event order |
| Store scheduling | Priority and polling can starve output | Wake-driven fair scheduling and measured bounded group commit |
| Model/context | Effective model limits are dropped | Persist `ResolvedModel`; incremental model-aware context plan |
| Run budgets | Caller-specific enforcement | Core-owned limits and typed terminal outcomes |
| Retry | Core/provider ownership can amplify | Delivery-certainty-aware attempt contract |
| Search/edit | Literal/exact-only contracts | Evaluation-driven ignore-aware search and patch edit |
| Terminal | One-shot only | Durable process handles, cursors, leases, monitors, optional PTY |
| Provider footprint | AWS SDK always linked through provider crate (resolved by H1) | Feature-gate heavy adapter families internally |
| Embedding | No profile/plan plane (resolved by H2) | `AgentProfile` plus cached `CompiledAgentPlan` |
| Protocol | Text-only and weak active control (resolved by H3) | Input parts, profile, plan digest, limits, steering, capabilities |
| Observability | Incomplete end-to-end evidence | Admission, compile, send, TTFT, persist, deliver, tool, replay spans |
| Event fan-out | Every subscriber re-reads SQLite and re-parses/re-serializes each event | Outbox returned from commit plus per-workspace broadcast; SQLite only for catch-up (D1) |
| Output commit | One fsync per output batch; no statement cache | Output-lane group commit with savepoints and deferred replies (D2) |
| Approval identity | Tool class derived from name strings; `ext__` falls to `Unknown` and executes | Classify from the catalog `EffectClass` (D4) |
| Per-run copies | History cloned per attempt; prompt rebuilt and hashed per run; schemas re-serialized | Shared `Arc` transcript, precompiled prompt prefix, `RawValue` schemas (D5) |
| Store identity | `ClaimedRun` cloned per event; three settlement paths; 332 source-erasing error maps | `Copy` `RunIdentity`, one `RunSettlement`, `PersistenceFault` (D9) |

### Reject

- Provider-name branches in request or stream hot paths.
- A provider-per-crate layout without measured build evidence.
- A second embedded agent runtime.
- Universal synchronous plugin hooks.
- Dynamic native-library loading.
- Unbounded listener, tool, provider, or child fan-out.
- Fire-and-forget addon loading with suppressed failures.
- Permission prompts presented as process sandboxing.
- Messaging, cron, voice, browser, or product memory inside core.
- An OpenAI-compatible facade as the source-of-truth protocol.
- A speculative distributed scheduler or cloud control plane.
- A broad tool catalog whose quality and latency have not won paired
  evaluations.

## Implementation Sequence

The work uses `H` task identifiers to avoid colliding with the existing
Terminal-Bench task numbering.

### Existing Roadmap Dependencies

The following are milestones owned by
[`terminal-bench-readiness.md`](./terminal-bench-readiness.md), not new tasks in
this plan:

| Milestone | Owning phase | Required outcome here |
| --- | --- | --- |
| R4 | Phase 4, linear and fair durable streaming | Streaming/reasoning persistence meets linearity, ordering, and fairness gates |
| R5 | Phase 5, resolved model, context, and spend | Effective model state and core-owned limit outcomes are available to plan compilation |
| R6 | Phase 6, tool tournament and terminal | Any richer search/edit/terminal contract has won its ablation and cleanup gates |
| R7 | Phase 7, sub-agent economics and scheduling | Child accounting/admission behavior is measured and remains bounded |
| R8 | Phase 8, warm runtime and request efficiency | Retry ownership, stable-prefix caching, and warm-path work meet their owning gates |

This document consumes completion evidence from those phases. It does not
redefine their schemas, migrations, tool contracts, or tests. One exception
is recorded explicitly: R8 lists retry exposure, shared immutable message
storage, and request-encoding benchmarks as unstarted P2 candidates. H14 (D3)
and H18 (D5) implement those three candidates here because the 2026-09-04
audit confirmed a measured amplification defect and a per-attempt history
copy on the default path. The readiness plan records the hand-off; R8 keeps
credential-lease caching, MCP bounds, and provider prompt-cache determinism.

### Task Index

| Task | Outcome | Depends on | Primary owner |
| --- | --- | --- | --- |
| H0 | Complete: current-runtime speed, size, RSS, replay, and concurrency baseline | None | `xtask`, existing benches |
| H1 | Cargo feature/dependency profiles, full/minimal baselines, and budget gates | H0 | Root, `qq-provider` |
| H2 | Shipped: immutable live `CompiledAgentPlan` and secret-free descriptor; cache repairs H25, H27 | H1, R5 | Root, config, core, provider |
| H3 | Complete: input parts, profiles, plan identity, limits, steering, capabilities, and correlation | H2 | `qq-protocol`, server, client |
| H4 | Fixtures complete; first external SDK deferred to a real consumer | H3 | `qq-client`, external adapter |
| H5 | Complete: declarative agent-pack manifests compiled into plans | H2 | Root, config, core |
| H6 | Complete: immutable tool catalog, progressive disclosure, skill index | H2 | Root, core, MCP, protocol |
| H7 | Complete: embedded external-tool host and shared host conformance | H6 | Core, MCP, root |
| H8 | Shipped: `ContextSource` and cache (in-tree consumers); admission/identity repair H28 | H2 | Core, root, protocol |
| H9 | Complete: post-commit observer loop and replay conformance | H3 | Protocol, client, server |
| H13 | Effect-classified approval: `ext__` tools obey every approval mode; MCP permit released before the connect wait (D4) | H6, H7 | `qq-core`, `qq-mcp` |
| H14 | Single retry owner in `qq-provider` with a plan-compiled `AttemptPolicy`; core turn retry deleted; amplification measured (D3) | H2 | `qq-provider`, `qq-core` |
| H15 | Published-event outbox and per-workspace broadcast; SQLite reads only for catch-up (D1) | H9 | `qq-core`, `qq-server` |
| H16 | Output-lane group commit with savepoints, deferred replies, and cached statements (D2) | H15 | `qq-core` store |
| H17 | Shipped schema 25: `runs.activity`, command counter, joined context load; claim to send in two hops (D6, D7) | H16 | `qq-core` store |
| H18 | Shared transcript `Arc`, precompiled prompt prefix, precomputed tool schemas (D5) | H14 | `qq-core`, `qq-provider` |
| H19 | Measured SSE framing optimization in provider and client (D10); borrowed views conditional on decoder-specific benefit | H18, decoder baseline | `qq-provider`, `qq-client` |
| H20 | Wake-driven control admission; polling loops removed; carried ≤20 ms output-fairness gate (D8) | H16, H23–H26 | `qq-core` |
| H21 | `RunIdentity`, `PersistenceFault`, one settlement path, `sessions.rs` split (D9) | H15–H17, H20 | `qq-core` |
| H22 | Bundled cold-path and structural fixes: config/auth load, protocol boxing and limits, route tables, TUI index and tail | None | Per crate |
| H23 | Supervised-child ownership across admission, overload, steering, and cleanup; implemented and validated on Linux, Windows qualification open | Supervised-delegation D4 | `qq-core` |
| H24 | Implemented: fresh child/audit admission, finite-spend serialization, owned-descendant receipts | Supervised-delegation D2 | `qq-core` |
| H25 | Live provider/MCP credential binding invalidation without secret-bearing durable identities | H2, H7 | Root, auth, MCP wiring |
| H26 | Bounded workspace-feed admission and lifecycle | H15 | `qq-core`, server fixtures |
| H27 | Active-generation accounting and atomic cache refresh admission | H2 | Root |
| H28 | Explicit context-source capacity rejection and immutable source identity | H8 | Core, protocol |
| HC1 | Headless admission and plumbing: `--correlation`, exclusive `--session` resume, shared `u32` turn limits, model-less `config check` | H3, H26 | Root, config, core, protocol |
| HC2 | Positive tool exposure through optional `policy.exposed_tools`; existing grants unchanged | H6, H13 | Config, core plan |
| HC3 | Typed final output: `--output-schema`, bounded repair turns, `final_output` on `outcome` and `RunFinished` | H3, HC1 | Protocol, core, root |
| HC4 | Headless golden fixtures per `PROTOCOL_VERSION` and compatibility statement | HC1–HC3 | Protocol tests, docs |
| H10 | First real OS process-sandbox adapter | R6, platform threat model | Core tools, root |
| H11 | Optional ACP/OpenAI compatibility facade | H4, real consumer | Adapter in existing surface owner |
| H12 | Crash, load, security, quality, and performance qualification | All shipped tasks and required R milestones | Workspace-wide |

### Phases 0–4 — Complete

Phases 0–4 shipped between 2026-09-01 and 2026-09-03. Their full receipts
(deliverables, acceptance evidence, test names, and measurement narrative)
were compressed on 2026-09-04; the complete text is in Git history at the
revisions below, and the reproducible measurement protocol is in
[`benchmarks/perf/README.md`](../../benchmarks/perf/README.md).

| Phase | Tasks | Completed | Revision | Landed |
| --- | --- | --- | --- | --- |
| 0 — Speed constitution | H0 | 2026-09-01 | `6383305` | `cargo xtask perf baseline/check`, versioned JSON reports, `benchmarks/perf/budgets-v1.json`, 47 metrics, 14 correctness receipts, deterministic fake-provider fixture with 1/10/100-session load and RSS sampling |
| 1 — Prerequisites and profiles | R4, R5, H1 | 2026-09-02 | `8ccba84` | R4 linear/fair streaming and R5 resolved model, context admission, `RunLimits`, and compaction hardening imported from the readiness plan; `provider-bedrock` feature gates the seven AWS crates; full and minimal profiles both pass the shared interface fixtures |
| 2 — Compiled plan | H2 | 2026-09-02 | `2375928` | `AgentProfile`, secret-free `AgentPlanDescriptor` with canonical digest (`DESCRIPTOR_VERSION = 1`), runtime-only `CompiledAgentPlan`, `SourceFingerprint` stat revalidation, opaque `CredentialEpoch`, root `PlanCache` (16 entries / 64 MiB, LRU, pinned active generations, single-flight); raw-secret hashing deleted |
| 3 — Backend contract | H3, H4 fixtures | 2026-09-03 | `27afe89` | Protocol 13, schema 21, descriptor 2: `InputPart`, `Correlation`, `AgentProfileId`, `RunPlanIdentity`, `SteerRun` and steering events, `SetSessionProfile`, expanded `RunLimits`, `ServerCapabilities` (`CAPABILITIES_VERSION = 1`), config `profiles`, typed client calls, 24 golden fixtures. External Python client deferred to a real consumer |
| 4 — Extensions | H5–H9 | 2026-09-03 | `5f48fd6` | Protocol 14, descriptor 3: immutable `ToolCatalog` with progressive exposure and `select_tools`; `ExternalToolHost` with an `EmbeddedToolHost` beside MCP and a shared conformance suite; `pack.ron` agent packs; bounded `ContextSource` with cache and fail policy; `qq-client::observer` post-commit loop; `ToolSpec` shared behind `Arc` |
| 5 — Correct the hot path | H13–H17 | 2026-09-04 | `70166bd` | Effect-classified approval (`ToolClass::Unknown` deleted, `ext__` tools gated); provider is the single retry owner (`AttemptPolicy`, core `'turn` loop and `TurnRetryPolicy` deleted, descriptor 5); published-event outbox and per-workspace broadcast; output-lane group commit with savepoints and a 128-statement cache; schema 25 (`runs.activity`, command counter, grouped snapshot accounting, two-hop claim, joined context assembly) |

Retained decisions from those receipts:

- Persist-before-publish, one terminal event per run, idempotent commands,
  and cursor replay are the invariants every later phase must keep.
- The warm plan path performs no filesystem discovery beyond the recorded
  `stat` list; refresh failures never poison a valid generation; active runs
  keep their admitted generation.
- Secrets and secret hashes never enter descriptors, digests, events, traces,
  snapshots, or cache diagnostics.
- Static built-in tools are the zero-overhead path and never enter the
  exclusion or pin paths.
- External hosts expose readiness and shutdown contracts; context sources
  expose budgets and failure policy; observers consume committed events.
  These are distinct interfaces. Source registration capacity and identity
  remain subject to the H28 repair.
- The Phase 4 recording host was shared and noisy; two tail-only metrics
  (`provider_delta_to_committed_core_event_ns` p95 and
  `http_cursor_reconnect_replay_ns` p95) were recorded as noise, not accepted
  regressions, after `main` failed the same budgets against itself.

Measurement trend at each phase boundary (clean detached 100-sample recorder
except Phase 4, which used the best p95 across repeated runs on a loaded
host). These are the pre-change baselines for Phase 5.

| Metric | Phase 0 | Phase 1 | Phase 2 | Phase 3 | Phase 4 |
| --- | ---: | ---: | ---: | ---: | ---: |
| Submit start to provider entry p95 | — | 16.4 ms | 16.5 ms | 17.0 ms | 13.5 ms |
| Durable direct command acknowledgement p95 | 3.5 ms | 5.8 ms | 5.8 ms | 5.9 ms | 3.6 ms |
| HTTP command acknowledgement p95 | — | — | 3.7 ms | 6.2 ms | 3.7 ms |
| Provider delta to committed core event p95 | 7.5 ms | — | — | — | 9.1 ms (noise) |
| Eight-stream output service gap p95 | — | 50.0 ms | 47.0 ms | 46.0 ms | 47.0 ms |
| 1 MiB / 512 KiB scaling ratio | 2.292x (red) | 1.951x | 1.919x | 1.895x | 1.892x |
| Cancellation to committed terminal event p95 | 9.2 ms | 59.1 ms (R5) | — | — | — |
| 100-session batch p95 | 7.77 s | — | — | — | 2.42 s |
| Release binary | 62.60 MB | 63.99 MB | 64.02 MB | 65.31 MB | 66.79 MB |
| Minimal release binary | — | 51.88 MB | — | — | 54.72 MB |
| Idle server RSS p95 | 16.06 MiB | 17.96 MB | 17.89 MB | 18.04 MB | 17.82 MB |
| `plan_compile` (embedded profile) | — | — | 20.9 µs | 21.5 µs | 23.9 µs |
| `plan_for` cold / warm | — | — | 180 / 6.1 µs | 180 / 6.2 µs | 202 / 7.7 µs |
| Compiled plan estimated heap | — | — | 7.0 KiB | 8.2 KiB | 14.0 KiB |
| `catalog_compile_512` | — | — | — | — | 1.21 ms |
| `select_tools_rank` (512 tools) | — | — | — | — | 18 µs |

Cold `plan_for` is dominated by configuration discovery and credential
resolution rather than plan construction, which is why H22 targets the
config and auth load paths. The eight-stream service gap has sat within 5 ms
of its 50 ms budget since Phase 1, which is what D2 addresses.

### Phase 5 — Correct The Hot Path

Status: complete 2026-09-04. Receipt follows; the original task text is kept
below it for the acceptance list.

#### Phase 5 Completion Receipt — 2026-09-04

Commits, in order: `ea5a6af` (H13), `d02a619` (H14), `e040cab` (H15),
`7ced6ca` (H16), `70166bd` (H17). Every commit passed `cargo fmt --check`,
`cargo clippy --workspace --all-targets --all-features -D warnings`, and
`cargo test --workspace`; H14 also passed the minimal
`--no-default-features --features test-support` profile.

Two deviations from the designs as written:

- D3 removed the retry field from the plan descriptor, which changes the
  canonical encoding; `DESCRIPTOR_VERSION` moved 4→5 and the golden digest
  was re-pinned. The plan had said "no descriptor bump"; the field was a
  constant default in every real plan, so no behavior changed.
- The audit columns landed as schema 24 after the audit was written, so D6/D7
  shipped as schema **25**, not 24.

Acceptance, item by item:

| Acceptance | Result |
| --- | --- |
| `ext__` denied under read-only, held under ask and supervised; MCP unchanged | Five-mode matrix with and without `read_only` hints, plus MCP decision tests, all green; hints never change a decision |
| Attempts per turn never exceed policy; amplification below 1.05 | `provider_retry_amplification_milli` = 1000 counts one core entry into a fake provider per logical turn, not transport sends. Separate `AttemptPolicy` tests cover pre-stream, pre-first-event, post-event, non-transient, `Retry-After`, and exhaustion |
| One catch-up read per subscriber, none per event; live and replayed streams byte-identical | Eight subscribers: 8 catch-up reads total, 0 during the run; `live.json == stored.json` for every event; lagged subscriber recovers via SQLite with no gap or duplicate |
| Failed outer commit fails every job and publishes nothing; control admitted between batches | Deferred-FK commit failure settles every job `Persistence` and the feed stays empty; a waiting control job runs within one group |
| Command ack within budget with a 512-event active session | `busy_workspace_command_ack_ns` median 3.02 ms / p95 5.9 ms with 560 events and an active run (gate 10 ms) |
| Eight-stream output service gap ≤20 ms | **Not met**: 29–45 ms (was 42–45). Completion dropped 0.91→0.29 s and the batch bench 236→138 ms. The remaining cause is unverified: persisted event timestamps do not distinguish queue, scheduler, and commit delay. The ≤20 ms gate is carried explicitly to D8 (H20); the existing executable budget remains 50 ms until the tighter target is achieved and qualified. |
| Claim to send uses two store hops; joined context equals previous assembly | Cancellation, file state, and steering ride the claim; the pre-change assembly is kept as a test oracle and matches on compaction, steering, and pruned-result stores |
| No protocol/capability bump; schema migration tested | `PROTOCOL_VERSION` 16 and `CAPABILITIES_VERSION` 1 unchanged; descriptor 4→5 (above); schema 24→25 with a backfill test |

Measurements (this host, release, medians unless stated):

| Metric | Before | After |
| --- | ---: | ---: |
| Retry ownership | Static worst case of up to 24 transport sends from nested retry owners | Measured 1.000 core entries into a fake provider per logical turn; actual transport attempts remain bounded by `AttemptPolicy` |
| Fan-out, delta to slowest of 8 subscribers | 13.7 ms | 12.1 ms |
| Fan-out, delta to slowest of 32 subscribers p95 | 26.4 ms | 14.9 ms |
| Command ack with 32 subscribers attached | 17.6 ms | 6.2 ms |
| R4 eight-stream completion | 0.91 s | 0.29 s |
| R4 eight-stream output service gap | 42–45 ms | 29–45 ms |
| R4 eight-stream cancellation to finished | 50–66 ms | 34–48 ms |
| `store_output_batch` (8×256×64 B) | 236 ms | 138 ms |
| Workspace snapshot | 899 µs | 465 µs |
| Submit start to provider entry | 9.8 ms | 9.3 ms |
| `provider_recipe_compile` | 537 ns | 549 ns |

New instruments: `provider_retry_amplification_milli`, six `fan_out_*`
metrics, `busy_workspace_command_ack_ns` (all budgeted), and the
`store_output_batch` bench in `qq-core`.

Deferred from this phase, now owned by Phase 6: deleting the 35 `notify`
call sites (the watch remains as a wake for `subagents.rs`; H22), the
`sleep(1 ms)` overload loops and the service-gap remainder (D8, H20), and
stale-result pruning by a stored tool kind rather than by name (H22).

#### Original Task Text


Implement H13, H14, H15, H16, and H17, in that order. H13 ships first
because it is a P0 approval bypass. H15, H16, and H17 touch the same store
functions and land serially; H14 is independent and may run in a parallel
worktree.

Benchmarks to record before each change, per the Performance Constitution:

- H14: a fake-provider fault-injection run reporting attempts per logical
  turn (the amplification counter);
- H15: subscriber fan-out at one, eight, and thirty-two subscribers on the
  existing SSE observation harness;
- H16: a `store_output_batch` bench (eight streams, 64-byte deltas) plus a
  rerun of the R4 fairness matrix; and
- H17: the H0 direct and HTTP acknowledgement proxies and the submit-to-
  provider-entry proxy, with a 512-event active session seeded.

Deliverables:

- `EffectClass`-driven approval with `ToolClass::Unknown` removed (D4);
- one `AttemptPolicy` on the compiled provider and no core turn retry (D3);
- `PublishedEvent`, `Committed<T>`, and a bounded per-workspace broadcast
  with SQLite catch-up (D1);
- output-lane group commit with savepoints, deferred replies, a prepared
  statement cache, and `RETURNING` sequence allocation (D2);
- schema 24 with `runs.activity`, a maintained command counter, grouped
  snapshot accounting, and a single joined context load (D6, D7); and
- migration and regression tests named in each design.

Acceptance:

- an `ext__` tool is denied under read-only and held under ask and
  supervised, and MCP decisions are unchanged;
- attempts per logical turn never exceed the policy and measured
  amplification is below 1.05;
- in steady state each subscriber performs exactly one catch-up read and
  no store read per event, and live and replayed streams are byte-identical;
- a failed outer commit fails every batched job and publishes nothing; a
  control job is admitted between batches;
- command acknowledgement stays within its budget with an active run that
  has 512 events;
- the eight-stream output service gap is at most 20 ms;
- claim to provider send uses two store hops and the joined context equals
  the previous assembly on the seeded fixture; and
- no protocol, capability, or descriptor version changes; schema moves
  23→24 with a migration test.

### Phase 5a — Repair Shipped Correctness Contracts

Implement H23, H24, H25, and H26 before Phase 6. H23 starts with the
supervised-child ownership invariant and its failure tests; H24 follows in
the owning delegation contract. H25 and H26 may be independently investigated,
but concurrent writing uses isolated worktrees and reviewed integration.

H23 is delivered in two bounded slices. The first prevents outcome-read
saturation or hard failure from allowing the parent to resume while its child
is still live.
The second establishes owned admission and cleanup across interrupting steering
and parent termination, including locally owned mutation/process quiescence.
A durable terminal event is not by itself proof of execution teardown. H23
requires both slices' failure and cleanup fixtures; this split does not defer
that acceptance to H12. Both slices now pass on Linux. Native Windows cleanup
qualification remains open and must not be inferred from Linux tests.

Each repair carries the acceptance fixtures in the follow-up audit table,
workspace checks, and its relevant latency/resource measurements. Do not
mark the tranche complete while a repair or its qualification remains open.
No broad refactor, new plugin surface, or global scheduling framework is part
of this tranche.

#### H23 Slice 1 Receipt — 2026-09-04

Implemented and locally validated; H23 and Phase 5a remain open. Child outcome
reads wait for a capacity wake on the bounded control lane while retaining
their child guard and writer permit. Ordinary command admission retains its
immediate overload response. Hard outcome-read failures fail the runtime and
signal child cancellation before the parent can continue normal tool execution.

Four regressions cover a live write child under actual control saturation,
unreadable child accounting, cancellation of a capacity waiter, and store close
waking waiters while draining accepted jobs. The runtime regressions failed
before their fixes. The saturation fixture acknowledges an actual admission
attempt while all 256 slots are occupied and checks the child's successful
result and durable completion before the parent's tool completion.

Validation: `cargo test --workspace` (1146 passed, 3 ignored),
`cargo fmt --all -- --check`, strict all-target/all-feature workspace Clippy,
and `cargo build --workspace` pass. The workspace tests require local loopback
access; this host's `NO_COLOR=1` was unset for existing color-output tests.
No protocol, descriptor, schema, or dependency changes.

Performance smoke: ten alternating baseline/candidate pairs of release
`xtask perf r4-worker --case eight-streams`, built from `036329a` and this
slice, on the same host/filesystem with no concurrent build or test run.
Values are median [min, max]; milliseconds except RSS in MiB.

| Metric | Baseline | H23 slice 1 |
| --- | ---: | ---: |
| Eight-stream completion | 278.7 [250.2, 341.7] | 274.1 [255.9, 309.4] |
| Control call latency upper bound | 20.09 [19.54, 24.07] | 20.92 [18.70, 30.98] |
| Cancellation to finished | 27.40 [25.86, 31.36] | 27.81 [23.44, 41.11] |
| Maximum output service gap | 24.0 [23, 28] | 24.5 [22, 35] |
| Peak temporary RSS | 9.02 [8.18, 9.87] | 9.12 [8.33, 9.80] |

Medians are close, but candidate control/cancellation/output maxima are higher;
the tail impact remains unresolved. This small comparison does not qualify
the full H0 regression gate or the carried H20 ≤20 ms service-gap target.
Repeat the controlled tail comparison before closing H23; the existing
absolute control/cancellation/gap budgets were not exceeded in these samples.

This first-slice receipt does not establish mutation/process quiescence.
Owned admission, steering, and teardown are covered by the second slice below.

#### H23 Slice 2 Receipt — 2026-09-04

Implemented and locally validated on Linux. Child admission now has a bounded
owner that survives dropped result waiters, retains started loader work and
permits, and awaits accepted creation through its reply. Interrupting steering
cancels and drains owned children before continuation. Child spend remains
keyed by tool-call identity until charged and synchronously acknowledged, so a
completed-but-unconsumed result is neither lost nor charged twice. Audit children
use the same ownership boundary; both interrupting and queued steering received
during an audit reach the next parent request.

Started runs drop dispatch and drain children, blocking file operations, and
owned shell tasks before terminal settlement can release the session. A native
write already inside atomic apply may finish; parent and queued replacement
writers wait for its exit. Shell tasks retain ownership through kill and reap;
a panic or failed termination/reap makes cleanup unconfirmed and fails the
runtime closed. Windows explicitly kills the owned child before waiting.
External MCP effects and detached or escaped processes remain uncertain;
interruption never implies rollback or authorizes an automatic retry.

Public-session regressions cover held creation replies, exact interrupted-child
spend, audit steering, failed cancellation persistence, unconfirmed process
exit, native writes held across steering/cancellation/reentrant cleanup/shutdown,
and shutdown during real blocking loader work. Tool regressions cover dropped
waiters, full shell-output queues, and panic cleanup. Independent Standards and
Spec reviewers challenged the ownership and failure cases; neither found a
remaining implementation blocker. H21 now explicitly owns making successful
teardown a structural prerequisite of the settlement interface.

Validation after integration with the concurrent provider/configuration commits:
workspace tests (1158 passed, 3 ignored), formatting, strict
all-target/all-feature workspace Clippy, and workspace build pass. Tests used
local loopback access with this host's NO_COLOR unset. No protocol, descriptor,
schema, or dependency changes. Native Windows tests were added but were not
executed on Windows.

Focused performance qualification: 30 alternating baseline/candidate pairs
per case, release workers on the same host/filesystem without concurrent builds
or tests. Baseline is 036329a; candidate includes both H23 slices. Values are
nearest-rank median / p95, matching the xtask convention, in milliseconds
except RSS in MiB.

| Metric | Baseline | H23 slices 1 + 2 |
| --- | ---: | ---: |
| Eight-stream completion | 286.81 / 320.69 | 285.16 / 311.12 |
| Control call latency upper bound | 19.51 / 25.37 | 19.06 / 24.51 |
| Cancellation to finished | 28.12 / 33.31 | 26.08 / 32.06 |
| Maximum output service gap | 24.00 / 32.00 | 23.00 / 29.00 |
| Eight-stream peak temporary RSS | 8.52 / 9.36 | 8.86 / 9.85 |
| One-MiB shell-output completion | 92.22 / 115.41 | 94.31 / 101.14 |
| Shell peak temporary RSS | 3.86 / 4.04 | 4.05 / 4.31 |

These cases stay within their existing relative and absolute p95 budgets.
The earlier sample's elevated candidate tails did not recur; RSS p95 increased
5.2% for streams and 6.6% for shell, within the 25% budget. This qualifies the
focused comparison, not the complete H0 suite. The carried H20 output-gap
target of at most 20 ms is still unmet (candidate p95 29 ms). Native Windows
teardown remains an explicit platform qualification gap. H24–H26 still precede
Phase 6; the following receipt records H24's subsequent repair.

#### H24 Receipt — 2026-09-05

Implemented in `qq-core`, with the owning D2 contract and architecture updated:

- Recompute cost and total/input/output-token allowance after each settled child.
  A finite spend bound serializes a child-containing turn; unbounded and
  duration-only read fanout keep their existing concurrency. Exhausted or unknown
  remainders refuse the next child by the affected family. Observed provider
  turns and reserved final responses may still overshoot their allowance.
- Carry the parent's absolute deadline through preparation, admission queues,
  and execution. Expired preflight creates no child; store creation already
  accepted may commit after expiry and is then cancelled under its retained
  owner. Cleanup may finish after the deadline.
- Include exact owned descendant runs in child spend receipts, excluding later
  user prompts in child sessions. Propagate unknown usage/cost independently;
  fail closed on missing, incomplete, malformed, or overflowing accounting.
  Preserve owned session history until every owning ancestor run settles.
- Give auditors the same remaining allowance, recheck budget exhaustion before
  parent completion, and record audit spend once in inclusive accounting.
  Positive known remainders are required only for imposed families; no minimum
  audit-cost prediction or general reservation scheduler was added.

Regression coverage includes two same-turn children, finite read fanout at one
and three configured child slots, all spend families, unknown/zero allowance,
preflight duration expiry, nested descendant spend, follow-up exclusion,
accounting corruption/overflow, owner-held deletion, and audit inheritance and
charge-once exhaustion. The two H23 injected-panic fixtures now distinguish a
terminated unreaped child from a live process using nonblocking wait on their
exact owned PID, after asserting production reports unconfirmed cleanup. Normal
successful-drain assertions are unchanged.

Local Linux validation: workspace tests (1170 passed, 3 ignored), formatting,
strict all-target/all-feature workspace Clippy, and workspace build pass. Tests
used local loopback access with this host's NO_COLOR unset. Independent Spec and
Standards reviews found no remaining implementation blocker. No wire protocol,
descriptor, persistence schema, or dependency changes; one benchmark target was
added using existing dependencies.

Focused measurement: 30 alternating baseline/candidate pairs for each latency
case, plus three pairs of a barrier-controlled read-overlap case. Baseline is
`f0a1c2e`; candidate code is `79d3211`. Both use the same new
`cargo bench -p qq-core --bench child_admission` fixture. Compiled plans and
store setup are outside its timer; providers add no artificial latency. Release
workers ran on the same Linux host/filesystem without concurrent review builds
or tests. Values are nearest-rank median / p95 in milliseconds, except RSS in MiB.

| Durable delegation case | Baseline | H24 |
| --- | ---: | ---: |
| Four unbounded read children | 145.60 / 154.51 | 147.05 / 171.95 |
| Four read children with finite cost/token caps | 145.22 / 260.90 | 150.96 / 294.95 |
| Child plus grandchild | 98.23 / 124.88 | 99.03 / 109.31 |

Finite-spend fanout has one active child at a time, versus three at baseline;
its median is 4.0% higher in this fixture. Real provider latency can increase
the serialization cost. Ordinary unbounded read fanout retains three active
children, and the barrier case confirms three overlapping descendant provider
streams in both versions. Every sample verifies expected child count, depth,
and inclusive spend. These new latency cases are descriptive measurements,
not an established numerical regression gate or proof of speed improvement.

| Existing default-path metric | Baseline | H24 |
| --- | ---: | ---: |
| Eight-stream completion | 272.80 / 292.07 | 269.95 / 298.37 |
| Control call latency upper bound | 19.00 / 22.46 | 18.86 / 21.57 |
| Cancellation to finished | 26.00 / 29.28 | 25.72 / 28.73 |
| Maximum output service gap | 23.00 / 26.00 | 23.00 / 26.00 |
| Eight-stream peak temporary RSS | 8.41 / 9.30 | 8.65 / 10.09 |
| One-MiB shell-output completion | 222.65 / 509.77 | 219.79 / 469.56 |
| Shell peak temporary RSS | 3.74 / 3.88 | 4.03 / 4.21 |

The eight-stream comparison passes existing relative, absolute, and noise
gates. All shell relative/absolute p95 checks pass, but the shell latency
noise check fails: median absolute deviation is 50.49% / 57.08% of the baseline /
candidate median, above the 50% gate. Both arms rose together from about 90 ms
to 200–500 ms during measurement; subsequent host I/O pressure was high. This
supports an interference hypothesis, not a code-regression conclusion. The
original noisy series is retained and does not qualify shell latency.

A bounded 60-second follow-up recorded 13 I/O-pressure snapshots and found no
stable low-pressure interval (at most 5% stall); final some/full 10-second
pressure was 29.21% / 27.46%. No repeat was run and no samples were discarded.
Shell latency qualification remains open for a stable host; code, correctness,
and streaming checks above are complete. Raw paired reports, binary hashes,
pressure observations, and verification logs are retained locally under
`target/qq-perf/h24-2026-09-05/` (untracked generated evidence).

This is not complete H0 qualification. H20's at-most-20-ms output service-gap
target remains unmet (candidate p95 26 ms), and H23 native Windows teardown
remains unqualified. The subsequent H25–H26 receipt follows.

#### H25–H26 Implementation And Qualification Receipt — 2026-09-06

Production changes through `6d31ba0` implement both repairs. H25 separates
secret-free durable identity from exact, redacted live provider/MCP bindings;
old active plans keep their handles while same-path key/header/bearer changes
compile new bindings. MCP eager connections start only after the second cache
admission check. Configuration source evidence is captured before probes and
reads, including permissions, explicit packs, and profile reloads, so a
concurrent edit cannot certify an old snapshot as current. H27 still owns
atomic replacement, retained-generation accounting, and key reclamation.

H26 validates and attaches a workspace feed in one store job before catch-up.
The receiver lease reclaims the feed after the final subscriber, including
cancelled admission and failed reply delivery. Replay/lag transitions retain
the lease and sequence deduplication; publishing into an inactive workspace
does not allocate a feed.

Independent architecture, correctness, and performance reviews found no
remaining implementation blocker. Regression tests reproduce stale provider
headers, duplicate eager MCP startup, pre-read source races, arbitrary-id
feed retention, cancellation, and replay/live ordering. On the integrated
production tree, the workspace suite passed **1194 tests, 3 ignored**;
formatting, strict all-target/all-feature Clippy, and workspace build passed.
These are local Linux checks, with no native Windows qualification claim.

The focused release comparison used clean baseline production `b0a8ce2` and
candidate `6d31ba0`, with exact copied worker hashes and source manifests.
All 310 workers completed their correctness checks. Five cache-process pairs
used 200 samples each; the remaining cases used 30 alternating process pairs.

| Observation | Baseline | Candidate |
| --- | ---: | ---: |
| Cold `plan_for`, median of process medians | 208.192 µs | 203.533 µs |
| Warm `plan_for`, median of process medians | 8.015 µs | 7.534 µs |
| 4096 rejected subscriptions, median / p95 | 44.594 / 61.727 ms | 19.341 / 38.316 ms |
| Retained RSS after churn, median | 135,016,448 B | 0 B (maximum 4096 B) |
| Fresh attach/replay, 900 raw samples, median / p95 | 16.220 / 58.130 µs | 21.500 / 72.617 µs |
| R4 eight-stream service gap, p95 | 35 ms | 28 ms |
| Shell completion, median / p95 | 90.617 / 112.192 ms | 89.364 / 113.413 ms |

Fresh attach/replay trades 14.487 µs at p95 for reclaimable ownership and has
no separate numeric budget. All focused R4 relative, absolute, and noise
gates passed, including shell latency; H20's stricter **≤20 ms** output-gap
target remains unmet. This is not the full H0 qualification.

The original fan-out comparison failed four relative gates: delivery p95 at
1/8/32 subscribers increased 39.1%/29.7%/25.0%, and 8-subscriber acknowledgment
increased 30.2%. Both arms also exceeded the 15 ms acknowledgment budget at
32 subscribers. Those failures remain recorded; host variability does not
waive them. Review found timed attachment assumptions and sequential terminal
observation in the fixture. Commit `5b77fee` replaces them with a FIFO
attachment barrier and concurrent observation, with two regression tests
and all 47 xtask tests passing. H0 fixture version **4** and focused feed
version **2** declare the measurement change. Both arms must be re-recorded
with identical corrected fixtures. That comparison is now complete: 30
interleaved A/B pairs and 30 same-binary A/A pairs, each worker contributing
five raw samples per metric. All 120 workers passed correctness checks.

| Corrected fan-out metric | Baseline p95 | Candidate p95 | A/B change | Same-binary A/A change |
| --- | ---: | ---: | ---: | ---: |
| Delivery, 1 subscriber | 6.904 ms | 6.975 ms | +1.02% | −27.39% |
| Acknowledgment, 1 subscriber | 3.526 ms | 3.863 ms | +9.54% | −40.97% |
| Delivery, 8 subscribers | 7.025 ms | 6.946 ms | −1.13% | −10.14% |
| Acknowledgment, 8 subscribers | 3.801 ms | 4.859 ms | **+27.85%** | **+68.83%** |
| Delivery, 32 subscribers | 6.478 ms | 8.177 ms | **+26.23%** | **+15.95%** |
| Acknowledgment, 32 subscribers | 3.516 ms | 3.541 ms | +0.72% | **+35.33%** |

The relative budget is 15%; bold values fail it. The corrected 32-subscriber
acknowledgment passes its 15 ms absolute budget. A/B medians differ by
−0.21% to +0.78%, and all MAD gates pass. The same-binary control failing
both candidate tail failures establishes non-repeatable tail qualification
in this recording; it neither waives the failures nor proves a production
regression. No samples were removed. Full version-4 H0 baseline/candidate
qualification and the native Windows teardown job remain required.

H20's pre-change diagnostic baseline is captured separately (`91c32ca`,
30 eight-stream and 30 controlled-saturation workers). Actual per-run output
commit gap p95 is 37.624 ms. For each worker's largest gap, queued service
has p95 31.171 ms; time between occupied store jobs has p95 0.045 ms and
output reply-to-caller-resume p95 is 0.085 ms. Repeated preceding output
commits dominate queue occupancy. These are instrumented attribution
measurements, not shipping-build gates; their separately ranked percentiles
must not be summed. They support H20 investigating bounded group formation
under control pressure. The stricter ≤20 ms target remains owned by H20.

#### Phase 5a Full H0 Comparison And Feed Hot-Path Receipt — 2026-09-07

The full version-4 H0 comparison ran against pre-H23 baseline `cebd13b` with
Phase 5a candidate `b5d94df` and failed four gates: minimal binary size
56,091,752 B against the 56,000,000 B absolute limit; `cursor_replay_ns`
p95 175.350 µs against 78.808 µs; fan-out delivery p95 at 1 and 32
subscribers +26.6% and +43.9%. A bounded replay probe found no additional
SQL, decoding, clones, or polls in the candidate, and both arms showed
two-speed behavior within a single process, so the tail failures could not be
attributed to H26. Symbol comparison of the two artifacts attributed most of
the size growth to H23/H24 closures already on `main`, and found the shipped
binary carried 12 MB of unstripped symbols with no release profile.

Rather than widen budgets, the feed hot path was redesigned. The
per-workspace `broadcast` channel is replaced by a bounded, sequence-indexed
ring retained only while subscribed. A cursor the ring covers attaches and
pages from memory with no store job; cold cursors validate and page from
SQLite in one control job that joins the ring before any later commit. Live
reads are cursor lookups, removing duplicate and gap handling. The ring is
bounded by 1024 events and 256 KiB of retained encoding; a non-contiguous
publish discards it rather than serve a hole. `Store::call` runs admission
and settlement in one non-generic body behind a thin generic shim, and
`ensure_workspace` uses the statement cache. The release profile now strips
symbols and builds with one codegen unit and thin LTO. Fifteen feed and
runtime regression tests cover warm and cold attach, tail seeding, lag
redirection, byte and count eviction, contiguity, wake registration,
last-subscriber release, and the drop/subscribe race.

Deterministic results: minimal artifact 55,770,448 → 38,734,176 B (−30.5%),
default artifact 67,845,536 → 45,479,728 B (−33.0%). The size budgets are
tightened to 41,000,000 and 48,000,000 B so the reclaimed headroom cannot be
spent silently. An in-process release probe of the exact `cursor_replay`
shape (nine events, 2000 iterations) measured median 22.6 → 0.87 µs, p95
38.2 → 1.7 µs, and 2001 → 1 store reads. In-process ring capacity change
raised R4 peak temporary RSS by 2–4 MB before the byte bound; after it, all
R4 RSS metrics are within +1%.

Three full H0 recordings on the shared host (baseline, candidate, baseline)
all produced fixture-version-4 reports with every correctness check passing.
The `perf check` exit remains non-zero: the same-binary A/A pair failed five
tail gates (all three fan-out acknowledgment tails, `long_stream`, and R4
restart reconstruction) and each A/B pair failed two to four, with the set
changing between runs and every failing metric showing identical medians and
a 3.2/6.2 ms bimodal acknowledgment tail. Host I/O pressure `some avg10` was
21–55% throughout. Candidate medians and p95 for `cursor_replay_ns`, all six
fan-out metrics, and both original R4 RSS gates are at or below both
baseline recordings. Tail qualification is therefore not repeatable on this
host in this window, consistent with the earlier A/A finding; the failures
are retained, not waived. The acceptance run must be repeated on a quiet
host before the tranche is marked complete. The focused
`feed_attach_replay` fixture subscribes from the workspace's initial cursor,
which the ring never covers, so it measures the cold path on both arms and
cannot gate the warm path; correcting that fixture is follow-up work.

### Phase 5b — Headless Contract For Supervisors

Status: proposed 2026-09-05. Design authority is
[`docs/design/headless-contract.md`](../design/headless-contract.md), which
records the current contract, the supervisor/QQ boundary, and the gaps this
tranche closes. CLI parsing and schema compilation are cold-path work. HC1
also changes startup ownership and shared limit types; HC3 adds opt-in core
validation, repair turns, and durable output. Independent work may proceed in
isolated worktrees alongside Phase 6, with coordinated integration through
review. HC1 and HC2 are independent. HC3 follows HC1 and coordinates settlement
and prompt identity with behavioral H21, H18, and H28; its behavioral changes
land before the mechanical `sessions.rs` split. HC4 lands last and pins the whole.

Motivation. A supervisor (batch runner, CI, evaluation harness, or hosted
service) consumes `qq run` through argv, `QQ_CONFIG_CONTENT`, JSONL stdout,
and the exit code. Today it must clamp `--max-turns` to `u16`, cannot stamp
its own identifiers into QQ events, cannot resume a persisted session from
the CLI, cannot narrow the tool catalog without editing configuration,
receives the final answer as prose, and re-reads QQ source at every bump to
confirm the record shapes. Each fix is generic: the same flags serve a
developer scripting `qq run` locally and the Harbor evaluation adapter.

Boundary rules for this tranche, restated from the design document:

- no supervisor-only mode, no product vocabulary (tenant, release, ledger,
  broker, billing) in flags, fields, or docs;
- QQ acquires no new authority: no money, no isolation claims, no tenancy;
- new JSONL fields are additive and optional; changed meaning or an expanded
  shared limit range needs a `PROTOCOL_VERSION` bump and new fixtures; and
- the default `qq run` path with none of the new flags preserves today's
  application payload after normalizing run identity, timestamps, build
  metadata, and declared version-field changes, checked by the HC4 fixtures.

#### HC1 — Headless Plumbing

Deliverables:

- `--correlation KEY=VALUE` (repeatable) on `qq run`, validated through the
  existing `Correlation::new` bounds (8 entries, 64/256-byte key/value,
  2 KiB total), passed on `CreateSession`, and echoed as `correlation` in the
  `trial` record. Duplicate keys and bound violations exit `2` before a
  session exists.
- `--session <SESSION_ID>` on `qq run`: establish exclusive ownership of the
  store before constructing `SessionRuntime`, which performs recovery and
  starts scheduling. A busy store exits `2` without opening the runtime or
  altering active work. Then resolve the workspace, verify the session
  belongs to it and is idle, and `SubmitPrompt` into it instead of
  `CreateSession`. `--correlation` with `--session` is an argument error. A
  running or missing session exits `2`. The `trial` record gains
  `resumed: true`. Only after ownership is established may recovery mark
  interrupted prior runs; resume never replays an uncertain side effect.
  Combining `--session` with `--profile` follows the same rule as the server's
  `SetSessionProfile`.
- `--max-turns`, `RunLimits.max_model_turns`, the core turn counter, and the
  `trial` record's `max_turns` widen together from `u16` to `u32`. The expanded
  shared wire range requires a `PROTOCOL_VERSION` bump and compatibility
  fixtures; this is not a CLI-only change.
- `qq config check` accepts a document with no effective model. Model
  selection is validated where it is consumed (`ask`, `run`, `serve`, TUI),
  which already report `invalid_configuration`. `config show` prints `model:
  <unset>`.

Acceptance:

- `qq run --correlation job=abc --correlation attempt=2` produces a `trial`
  record and a workspace snapshot containing exactly those entries;
  `--correlation` violating any bound exits `2` with no session row written.
- `qq run --session <id>` against an idle persisted session appends a second
  run to the same `session_id`, `trial.resumed == true`, and replay from
  cursor 0 shows both runs in order; against a running session it exits `2`
  and the running session is unaffected; against a foreign workspace's
  session it exits `2`.
- A two-process fixture holds a live run in a shared store while another
  `qq run --session` attempts resume: the second exits `2` before recovery,
  leaves the active run, preparing ownership, and events unchanged, and
  cannot claim queued work. A crashed owner's store can be reopened and
  recovered without replaying uncertain tools.
- `--max-turns 70000` is accepted and enforced (fake provider, 70000 turns
  not reached; a cap of 3 still settles `budget_exhausted` at turn 3).
- `QQ_CONFIG_CONTENT='(version: 1)' qq config check` exits `0`;
  `qq ask` under the same environment exits `2` with the existing "model must
  be configured" message.
- Historical limit records decode unchanged; boundary fixtures cover 65535,
  65536, and 70000 with no counter truncation. The protocol version bumps for
  the widened range; optional `trial` fields alone need no descriptor or
  store-schema change. HC1 and HC3 may share a protocol bump only if their
  wire changes integrate atomically; separate landings each retain their
  required version and fixture changes.

#### HC2 — Positive Tool Exposure

Implementation receipt, 2026-09-06: `93ef6b8` adds the optional restriction
without changing approval grants or the descriptor version. Configuration
validates names and the 1024-name/duplicate bounds, intersects layers before
trust-sensitive grants, and compilation intersects the result with the
profile/pack catalog. An omitted selector makes admitted external tools
directly callable under existing schema/catalog bounds.

Genuine regression failures preceded the implementation. The local workspace
suite passed 1207 tests (3 ignored), strict workspace Clippy, formatting, and
workspace build. A subsequent independently requested oversized-tool
execution regression passed separately, followed by strict core Clippy.
Architecture and correctness reviewers reconciled namespace admission and
typed catalog exclusions and closed their findings. The branch then rebased
onto the Phase 5a fixture-version correction without production conflicts;
combined integration and default-path H0 qualification remain pending.

Compatibility interpretation: add `policy.exposed_tools` for catalog
narrowing. Existing `policy.allow_tools` keeps its grant semantics and layer
composition; managed `deny_tools` and `deny_shell_prefixes` keep filtering
grants. Exposure does not grant execution authority, and grants cannot
restore a tool excluded from the catalog.

Deliverables:

- Optional `policy.exposed_tools: [..]` in configuration. When present, the
  effective catalog is the intersection of this list and the existing
  profile/pack exposure. An absent field adds no restriction; an empty list
  exposes no tools. Layers intersect (a narrower layer cannot widen),
  matching `allowed_providers` composition. Existing managed grant denies
  do not become catalog filters; profile/pack tool policy and the new explicit
  exposure field own that behavior.
- MCP names use the existing `mcp__<server>__<tool>` form; static built-ins
  are named as today. `config check` validates exact static names and MCP
  name syntax without requiring a model or discovering MCP tools. Plan
  compilation validates discovered MCP-name membership among profile-admitted
  servers before applying the exposure filter. Configured servers excluded
  by the profile's MCP subset remain excluded without discovery. An unknown
  namespace or missing member on an admitted server is a configuration error
  before provider work. Known tools still pass ordinary schema/catalog bounds;
  their existing typed exclusions remain visible and valid peers stay usable.
- `qq run --profile <name>` is documented as the primary way a supervisor
  selects a pre-compiled exposure; `--allow-tool` and `--allow-shell` keep
  their existing grant semantics and are re-documented as *widening held
  calls*, never as an allowlist.
- The plan descriptor already digests the catalog; a change to the effective
  exposure therefore changes its digest with no descriptor version change.
  Different declarations that produce the same effective catalog need not
  have different digests.

Acceptance:

- `exposed_tools: ["read_file", "search"]` under `--approval full` exposes
  exactly two tools in the model request (fake provider asserts the schema
  list); a call to `edit_file` returned by the fake provider is a tool error
  ("unknown tool"), never an approval hold.
- A global `exposed_tools` of five tools and a project `exposed_tools` of two
  yield two; the project cannot re-add a globally excluded tool.
- `exposed_tools: ["nonexistent"]` or malformed MCP names fail `config
  check` naming the entry. A syntactically valid MCP name can pass model-less
  `config check`; a name absent from the discovered catalog fails plan
  compilation before provider work.
- An absent `exposed_tools` preserves existing catalogs and grant behavior;
  an empty list exposes no tools. Neither a grant nor a broader layer can
  restore a tool excluded by profile/pack policy or an exposure intersection.
- Approval matrix from H13 is unchanged for every exposed tool, including
  existing `policy.allow_tools`, `policy.allow_shell_prefixes`, and managed
  grant-deny fixtures.

#### HC3 — Typed Final Output

Deliverables:

- `--output-schema <PATH>` on `qq run` (JSON Schema draft 2020-12 subset:
  object/array/string/number/integer/boolean/null, `required`, `enum`,
  `properties`, `items`, `additionalProperties: false`; anything else is
  `invalid_configuration`). The schema is digested into the plan descriptor
  (`DESCRIPTOR_VERSION` bump with fixture re-pin) and its text becomes part
  of the system-prompt suffix so `RunPromptIdentity` reflects it.
- Read and compile the schema once off Tokio workers into the immutable
  plan. Bound the schema to 64 KiB, 32 nesting levels, and 4096 JSON values
  (including enum values); reject excess before a session exists. Reject
  all references, including `$ref`, and perform no network discovery or
  external-reference fetches. Validation consumes the already bounded
  assistant output. Each validation-error payload, including rendered
  feedback or the durable result, is at most 8 KiB; the repair cap bounds
  repeated feedback.
- When present, the run's final assistant message must be a JSON document
  satisfying the schema. On failure QQ appends a validation-error user turn
  and retries up to `--output-repair-turns N` (0–8, default 2, counted against
  all ordinary run budgets, including `max_model_turns`). The repair allowance
  applies to the whole run and is not reset by audit revisions or steering.
  Exhaustion settles `task_failed` with a typed
  `final_output: { status: "invalid", errors: [..] }`.
- `RunFinished` and the JSONL `outcome` gain an optional `final_output`
  field: `{ status: "valid", value: <json> }`, `{ status: "invalid",
  errors }`, or absent when no schema was given. Persist the result before
  publishing `RunFinished`; replay reconstructs it unchanged. Protocol bump;
  the server and client surface it without interpretation.
- Validate the final answer after any audit revision, retaining D5's
  existing bounded audit/revision lifecycle. Schema repairs cannot introduce
  an additional audit loop. Cancellation and steering use the existing core
  lifecycle; validation never turns an interrupted or exhausted run into a
  successful typed result.
- Providers with native structured-output support (OpenAI `response_format`,
  Google `responseSchema`, Anthropic tool-forced JSON) may use it behind the
  existing provider-neutral request; validation still runs in core because
  provider guarantees are not uniform. No provider-name branch enters the
  hot path; the capability is a compiled-plan field.

Acceptance:

- Fake provider returns valid JSON first try: `outcome.final_output.status
  == "valid"`, exactly one model turn.
- Fake provider returns prose then valid JSON: two turns, valid.
- Fake provider returns prose three times with `--output-repair-turns 2`:
  `task_failed`, `final_output.status == "invalid"`, three turns, exit `1`.
- Repair turns count against `--max-turns`; `--max-turns 2` with two
  failures settles `budget_exhausted`, not `task_failed`.
- Token, cost, duration, and reserved final-response limits remain authoritative
  during repair. Cancellation or steering during a repair request drains
  through the ordinary ownership path; neither resets the repair allowance.
- Audit revision followed by schema repair remains within both existing audit
  bounds and the per-run repair bound; an invalid final answer cannot publish
  a valid result. Replay retains the exact settled `final_output`, and a
  persistence failure cannot publish it as durable.
- An unsupported schema keyword or reference, schema byte/depth/node excess,
  or repair count above 8 exits `2` before a session exists. Boundary fixtures
  cover the accepted limits and error output remains at most 8 KiB.
- Same prompt with and without a schema yields different
  `AgentPlanDigest` and different `RunPromptIdentity`.
- Golden `provider_encode` and `plan_compile` benches show no change on the
  default (schema-less) path. Record enabled schema-compilation and validation
  latency/allocation plus a deterministic repair trajectory before accepting
  HC3; include the maximum schema and bounded error cases.

#### HC4 — Golden Fixtures And Compatibility Statement

Deliverables:

- `crates/qq-protocol/tests/fixtures/headless/v<PROTOCOL_VERSION>/`
  containing one `trial`, a representative `event` sequence (text, tool
  call, child event, steering), and one `outcome` per status, generated by a
  deterministic fake-provider run and checked in.
- A test in the root package that runs `qq run --format jsonl` against the
  fake provider and asserts byte equality with the fixtures after
  normalizing ids, timestamps, `workspace_identity`, digests, and
  `qq_version`/`qq_source_revision`. Each version's fixture asserts its exact
  `protocol_version`; that field is not normalized within a version.
- A test that decodes every fixture with `serde_json::Value` and asserts the
  required-field set from `headless-contract.md` is present, so a supervisor
  can copy the assertion.
- The compatibility policy section of `headless-contract.md` is referenced
  from `protocol.md` and `README.md`; `PROTOCOL_VERSION` bumps require a new
  fixture directory and retain the previous one.

Acceptance:

- The fixture test fails when any record shape, field name, or `status`
  string changes without a fixture update.
- Across versions, the default-path fixture (no HC1–HC3 flags) preserves
  application payload after the normalization above and declared version-field
  changes. Assert those version changes separately and retain both fixture
  versions; optional output fields remain absent without their flags.

Phase 5b is complete when HC1–HC4 have their acceptance fixtures green,
`headless-contract.md` has moved each gap row from "Intended" to "Shipped"
with the commit, and the workspace gates pass. The default-path H0 regression
gate remains required. HC3 additionally records its enabled compilation,
validation, and repair measurements; the cold-path classification does not
exempt opt-in runtime work from performance and resource acceptance.


### Phase 6 — Finish Fairness, Shrink Per-Run Work, And Consolidate

After Phase 5a, implement H20 first. Then land the behavioral settlement
portion of H21, H27–H28, and H22's explicit-pack revalidation and MCP-name
validation fixes. H18 follows those correctness repairs. H19 follows its
decoder baseline and only ships the design justified by that evidence. The
mechanical H21 file split and remaining structural H22 items land last,
separately from behavior changes.

Benchmarks to record before each change:

- H18: `provider_encode` (one MiB plus 32 schemas with a counting
  allocator) in `qq-provider`, plus reruns of `provider_compiler`,
  `plan_compile`, and the `plan_for` warm path;
- H19: `sse_decode` at 64 KiB, 512 KiB, and one MiB in `qq-provider`, plus
  allocations and latency through a deterministic local HTTP/SSE pipeline;
- H20: cancellation under 256 queued control jobs and the eight-stream mixed
  control/output fixture, with queue admission/dequeue/commit timing; and
- H22: the H0 cold `plan_for` measurement and the TUI 200-session sidebar
  case.

Deliverables:

- `Arc<Vec<Message>>` transcripts, precompiled prompt prefix and digest
  state, and precomputed schema measurement and `RawValue` schemas (D5);
- if H19 ships, measured SSE framing improvements in provider and client;
  borrowed event views and shared tool ids only when the decoder baseline
  supports D10;
- `control_slots` admission and the removal of every `sleep(1 ms)` retry
  loop and store poll (D8);
- `RunIdentity`, `EventContext` constructors, `RunSettlement`,
  `PersistenceFault`, and the `sessions.rs` split as a separate commit after
  HC3's behavioral changes (D9);
  and
- the bundled fixes listed under H22.

Acceptance:

- one MiB request heap is at most 2x the payload and encode is at most
  10 ms; the prefix-plus-suffix prompt digest equals the full digest;
- the one MiB to 512 KiB scaling ratio stays at or below 2.2x and improves
  against the Phase 4 receipt;
- cancellation is at most 100 ms with 256 queued control jobs and no site
  polls the store;
- the carried eight-stream output service gap is at most 20 ms under mixed
  control/output load, with no relaxation of cancellation or durability;
  once qualified, tighten its executable budget from 50 ms to 20 ms;
- if H19 ships, it improves decoder-specific allocation/latency measurements;
  the fake-provider stream-scaling ratio alone cannot qualify it. A documented
  no-change decision is acceptable when measurements show insufficient benefit;
- active and superseded plan generations obey entry/byte limits; rejected
  refresh leaves the previous cached generation intact, including an
  equivalent-plan refresh whose source evidence grows. Completed per-key
  compile guards are reclaimed under distinct-key churn without admitting
  concurrent same-key compiles; explicit configuration in request keys is
  compared privately, redacted in diagnostics, and never hashed;
- excess required context sources fail compilation explicitly, and changing
  source identity, version, budget, or fail policy changes the plan digest;
- settling an already-settled run through any path is a no-op and every
  `PersistenceFault` variant is reachable in tests;
- the `sessions.rs` split changes no behavior and lands as its own commit;
- explicit pack manifests are revalidated on warm hits and pack MCP names
  are validated;
- the H22 route-table equality test passes between client and server; and
- the default path stays within the regression gate for every metric in the
  Phase 4 receipt.

### Phase 7 — Upgrade Execution Quality And Isolation

R6-R8 remain owned by the readiness roadmap. Implement H10 only after R6 has
selected and shipped a real terminal/process contract and a platform threat
model defines the isolation boundary. This document does not redefine the
search, edit, terminal, sub-agent, scheduling, or warm-runtime contracts.

Acceptance:

- the owning readiness plan records completion evidence for each R6-R8
  milestone required by a shipped extension;
- sandbox tests prove filesystem, network, process, and secret boundaries;
- the local and sandbox adapters pass one shared process contract suite;
- sandbox failure never silently falls back to unsandboxed execution;
- terminal/process cancellation, timeout, shutdown, and recovery leak no
  processes; and
- the sandbox adapter remains optional and feature-gated where its platform
  dependencies are not needed.

### Phase 8 — Add Product Adapters On Demand

Implement H11 only for an actual client.

Possible adapters:

- ACP;
- OpenAI-compatible HTTP;
- messaging gateway;
- cron/scheduling service;
- voice/TTS application;
- browser or desktop client; and
- a product-specific memory service.

Acceptance:

- the adapter uses `qq-client` or the native HTTP protocol;
- it introduces no alternate runtime or direct store access;
- native QQ events remain authoritative;
- capability loss in the compatibility protocol is documented and tested;
- product auth and tenancy remain outside `qq-core`; and
- the adapter can be disabled without affecting the base binary's hot path.

### Phase 9 — Qualification

H12 qualifies the complete story, not only individual modules. Each preceding
repair already includes its own fault, cancellation, and recovery fixtures;
H12 is the combined-system qualification, not their first execution. It re-runs
every Phase 5 and Phase 6 pre-change baseline and enforces the recorded
improvements as regression gates in `benchmarks/perf/budgets-v1.json` (or a
successor budget file).

Required scenarios:

- cold and warm direct/TUI/server execution;
- one, ten, and one hundred concurrent sessions;
- long text and reasoning streams;
- provider connection failure before send and ambiguous failure after send;
- store saturation, disk-full, corruption, migration, and restart;
- client disconnect/reconnect during text, tool, approval, and terminal work;
- addon discovery failure, refresh failure, crash, timeout, overload, and
  shutdown;
- context-source slowness and stale cache;
- MCP and embedded tool conformance;
- cancellation at every provider/tool/sub-agent boundary;
- terminal process cleanup;
- sandbox escape attempts;
- same-model agent-quality comparison; and
- minimal/full binary, RSS, startup, and latency gates.

## Verification Strategy

### Unit And Contract Tests

- Provider request/stream fixtures for every protocol and delivery state.
- Stable plan-digest and invalidation fixtures.
- Manifest parsing, precedence, provenance, trust, and capability tests.
- Tool-host and context-source conformance tests.
- Exhaustive run-limit and terminal-outcome tests.
- Protocol compatibility and unknown-field tests.
- Queue, output, concurrency, and cancellation bounds.

### Durable Integration Tests

Use fake providers, temporary SQLite stores, and temporary workspaces to prove:

- commands are idempotent;
- events publish only after commit;
- accepted runs have one terminal event;
- restart does not repeat possibly-executed tools;
- plans and profiles remain stable across config changes;
- child ownership and authority survive restart;
- external tool/context failures settle deterministically; and
- cursor replay reconstructs the same client state.

### Performance Tests

- Provider compiler benchmark.
- Plan compilation and cache benchmark.
- Model-request encoding benchmark (`provider_encode`: one MiB plus 32
  schemas with heap accounting; added by H18 before the change).
- SSE framing and decode benchmark (`sse_decode` at 64 KiB, 512 KiB, and
  one MiB; added by H19 before the change).
- Streaming append and reasoning batching benchmark, plus the
  `store_output_batch` group-commit bench (H16).
- Store fairness and replay benchmark, plus subscriber fan-out at one,
  eight, and thirty-two subscribers (H15).
- Fake-provider attempt-amplification counter (H14).
- Cancellation under 256 queued control jobs (H20).
- Static versus MCP versus embedded tool-dispatch benchmark.
- Context-source cold/warm benchmark.
- Persistent terminal operation benchmark.
- TUI render benchmark.
- Concurrent-session and child-fan-out load test.

### Quality Evaluation

Run same-model, same-prompt paired evaluations for tool, context, delegation,
and compaction changes. Record:

- verified success;
- wall-clock time;
- provider turns and tool calls;
- input/output/cache/reasoning tokens;
- estimated cost;
- harness failures;
- context overflow or compaction events; and
- task-specific correctness evidence.

Do not accept a microbenchmark win that reduces verified success or merely
moves work into additional model turns.

### Workspace Gates

For implementation phases, run the narrowest relevant tests while iterating,
then the applicable workspace gates:

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace
```

Run provider compilation benchmarks when provider construction changes. Run
new full/minimal performance gates whenever feature flags, addon compilation,
streaming, persistence, context, tool dispatch, or terminal execution changes.

## Risk Register

| Risk | Failure mode | Mitigation |
| --- | --- | --- |
| Universal plugin abstraction | Every run pays dynamic discovery/hook cost | Separate deep extension lanes and immutable plans |
| Addon trust confusion | Declarative package gains accidental code authority | Manifests contain data; executable behavior uses explicit native/MCP/embed boundaries |
| Cache invalidation | Run uses stale prompt/tool/policy | Digest every behavior-affecting input and retain provenance |
| Reload races | Active run observes partially updated catalog | Compile new generation off-path and atomically swap for later runs |
| Provider feature flags | Full and minimal behavior diverge | Shared contract fixtures and CI feature matrix |
| External tool overload | Runtime tasks or output grow without bound | Per-host and global permits, deadlines, quotas, bounded queues/output |
| Context latency | Retrieval dominates TTFT | Cache/prefetch, strict budgets, explicit fail policy, trace separately |
| Observer coupling | Analytics/memory delays output | Consume committed events asynchronously with cursors |
| Retry ambiguity | Duplicate billed request or side effect | Delivery certainty and provider idempotency where supported |
| Compatibility facade | Lowest-common-denominator API becomes architecture | Native QQ protocol remains authoritative |
| Terminal complexity | Leaked processes or platform divergence | One-shot shell stays fast; durable supervisor is bounded and qualified |
| Sandbox claims | Policy UX mistaken for isolation | Call it a sandbox only after adversarial platform tests pass |
| Scope expansion | Backend absorbs Hermes product features | Keep product adapters above `qq-client` |
| Benchmark gaming | Startup/size improves while outcomes regress | Measure useful-result latency, reliability, cost, and resource use together |
| Group commit | A batched job is acknowledged before the outer commit, or one failing job fails its siblings | Deferred replies fire only after commit; savepoint per job; crash test before commit leaves nothing durable |
| Broadcast handoff | A subscriber misses or duplicates an event between SQLite catch-up and the live feed | Catch up until the first buffered sequence, then drain; `Lagged` returns to catch-up; no-gap ordering tests |
| Retry ownership move | Deleting the core loop drops a failure class the provider cannot see | Provider restarts only before the first yielded event; fake-provider fixtures cover pre-stream and mid-stream faults |
| Prompt prefix memoization | Prefix plus suffix digest diverges from the full digest and changes persisted prompt identity | Digest-equality fixture is a Phase 6 acceptance gate |
| Consolidation churn | Mechanical splits and error-type changes hide behavior changes | D9 lands the behavior change and the file split as separate commits |

## Architecture Review Questions

Before each new interface is accepted, answer:

1. What are the two real consumers that justify the seam?
2. What complexity does the module hide from its callers?
3. Can the behavior be compiled or cached outside the run hot path?
4. What is the queue, output, concurrency, retry, and shutdown bound?
5. What authority does the component receive?
6. What happens if it is slow, unavailable, invalid, or crashes?
7. Is its output authoritative, advisory, or observational?
8. How is the exact version/digest retained for replay and diagnosis?
9. What benchmark or evaluation proves its value?
10. Can the component be disabled without changing default-path behavior?

If only one hypothetical consumer exists, keep the behavior concrete. Extract
the interface when the second adapter makes the shared contract real.

## Definition Of Done

The speed-first extensible backend is complete when:

- QQ's full and minimal builds have enforced binary, startup, RSS, and latency
  budgets;
- long streaming and reasoning persistence meet linearity and fairness gates;
- effective model capabilities and core-owned run limits are durable and
  visible through every interface;
- the native protocol supports structured input, profiles, steering,
  capabilities, replay, and typed outcomes;
- a real external product can use QQ through a thin client without spawning a
  fresh process per turn or importing QQ internals;
- `CompiledAgentPlan` removes discovery/configuration work from warm runs and
  records reproducible plan identity;
- unused provider families and addon mechanisms impose no minimal-build or
  default-hot-path cost beyond accepted gates;
- agent packs, MCP, one embedded tool host, one context source, and one
  post-commit observer pass conformance and failure tests;
- every queue, task, process, retry, output, and concurrency dimension is
  bounded, and retry has exactly one owner;
- event delivery serializes each committed event once and reads the store
  only for catch-up, and output persistence commits in bounded groups;
- every tool call is approval-classified from catalog effect data rather
  than its name;
- terminal and sandbox behavior, if shipped, passes cleanup and adversarial
  tests on supported platforms;
- crash/restart never repeats uncertain side effects and every accepted run
  settles durably;
- same-model evaluations show that shipped tools, context, and delegation
  improve verified work per dollar and minute; and
- product integrations remain clients of one durable QQ runtime.

Until those conditions are met, the immediate implementation boundary is
Phase 5a: qualify the implemented H25 live credential binding and H26
workspace-feed lifecycle, with H23 native Windows qualification still open. Phase 6
then starts with H20 and the early correctness repairs (behavioral H21,
H27–H28, correctness H22), followed by H18, measured H19, and mechanical
consolidation. Independent Phase 5b (HC1–HC4) work may run in parallel
worktrees from H26 onward. HC3's opt-in runtime changes coordinate with
settlement and prompt identity and land before the mechanical split. Phase 5
shipped on 2026-09-04 with its output-service-gap gate still open. Plugin or
marketplace work is not the
next slice.
