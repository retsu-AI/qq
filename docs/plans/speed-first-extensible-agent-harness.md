# Speed-First Extensible Agent Harness Backend

## Status

| | |
| --- | --- |
| Now | Phase 6 — H20 in review (`ab6de6f`, `d05e474`); next behavioral H21, then H27, H28, correctness H22 |
| Next | Phase 6 continued (H18, measured H19, mechanical H21 split, structural H22); Phase 5b HC1/HC3/HC4 in parallel worktrees |
| Open gates carried | Eight-stream output service gap ≤20 ms at p95 (median met by H20; executable budget stays 50 ms until a quiet-host p95); Phase 5a full H0 tail acceptance on a quiet host; native Windows teardown beyond the targeted CI job |
| Last closed | H20 implementation, 2026-09-09 (`d05e474`, ADR-0011); Phase 5a feed hot-path redesign and release profile, 2026-09-07 (`893e582`) |
| Versions | `PROTOCOL_VERSION` 16, `CAPABILITIES_VERSION` 1, `DESCRIPTOR_VERSION` 5, store schema 25, H0 fixture version 4 |

Updated 2026-09-09. The `Now` row is authoritative for what is being worked;
update it in the same PR that ships or reprioritizes work.

This plan defines how QQ becomes an extremely fast, lightweight, customizable
agent harness that can serve as the backend for products such as a
Hermes-style personal agent. It is a backend plan, not a plan to copy every
product surface from Codex, OpenCode, Pi, fx, or Hermes into QQ.

The central decision is:

> Compile customization once, execute directly in the hot path, persist before
> publishing, and keep every queue and concurrency boundary explicit.

QQ is a small durable execution kernel with a compiled customization plane.
Messaging gateways, cron, voice, browser automation, product identity, and
user-facing memory products remain clients of that kernel.

## Authority And Companions

This document is the active backend plan and a companion to the architecture.
It does not silently override the design documents.

- [`docs/design/architecture.md`](../design/architecture.md) is the system
  boundary and dependency-direction source of truth. Shipped designs from this
  plan (compiled plans, tool catalog, hosts, packs, context sources,
  observers, feed ring, group commit, provider-owned retry, effect-classified
  approval, `runs.activity`) are documented there, not here.
- [`docs/design/harness-audit-2026-08.md`](../design/harness-audit-2026-08.md)
  holds the reference audit (method, snapshot hashes, cross-project feature
  inventory, per-project findings, Hermes product boundary) that motivated
  this plan.
- [`docs/design/headless-contract.md`](../design/headless-contract.md) owns the
  `qq run` JSONL/exit contract, the supervisor boundary, and the HC1–HC4 gap
  table implemented as Phase 5b here.
- [`terminal-bench-readiness.md`](./terminal-bench-readiness.md) owns
  tool-contract ablations, terminal qualification, sub-agent economics, the
  remaining warm-runtime candidates, and the Terminal-Bench program (R6–R8).
- [`supervised-delegation.md`](./supervised-delegation.md) owns continuation
  on truncation, the delegation roster, supervised write children, the
  final-answer audit, and the pending paired evaluation. H23 and H24 landed
  through its D4 and D2 contracts.
- [`run-snapshots.md`](./run-snapshots.md) and
  [`lsp-diagnostics.md`](./lsp-diagnostics.md) own their named concerns.

Where this plan depends on one of those contracts, implementation lands
through the owning plan and this document records the dependency rather than
duplicating the design.

The architecture defers a public extension interface, a plugin marketplace,
JavaScript packages, multi-user tenancy, and distributed workers. Later phases
may cross those boundaries only after their entry gates are met and the
architecture document or an ADR records the decision.

## Goals

QQ should maximize verified successful agent work per dollar, per minute, and
per unit of local resource use while remaining pleasant to embed and extend.
Correctness, durability, and safe execution are baseline constraints rather
than tradeable performance features.

An application developer should be able to: create or resume a durable
session; select a versioned agent profile; submit an idempotent multimodal
command with explicit resource limits; receive durable progress through a
reconnectable event stream; respond to approvals or steer an active run; add
tools through MCP or a trusted embedded host; add instructions and skills
through declarative agent packs; add a provider without changing the agent
loop; add memory retrieval without intercepting token streaming; and recover
from client or process loss without repeating uncertain side effects.

The default path pays only for the behavior it uses: configuration,
discovery, trust, schema preparation, and provider selection happen before the
run hot path; disabled adapter families add no shipping dependency to minimal
embedders; active runs use immutable shared plans and direct dispatch;
observers consume committed events asynchronously; every queue, buffer, retry,
task, subprocess, and fan-out is bounded; and every material optimization is
supported by an end-to-end measurement.

Customization uses a small set of deep extension lanes with different trust
and performance contracts rather than one shallow interface: declarative agent
packs; compiled provider adapters; static native tools; external or embedded
tool hosts; bounded context sources; post-commit event observers; and surface
adapters using the versioned client protocol.

## Non-Goals

This plan does not authorize:

- a universal `Plugin` trait through which every token, event, and tool call
  passes, or arbitrary synchronous pre/post hooks around provider deltas, tool
  output, persistence, compaction, or session lifecycle;
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
  conflict semantics exist. One serialized `Supervised` write child per run,
  every held action adjudicated, is permitted by `supervised-delegation.md`;
- WebSocket, gRPC, GraphQL, or a binary wire protocol without measurement or a
  feature that HTTP/SSE cannot express;
- an OpenAI-compatible facade as the source-of-truth protocol;
- unbounded queues, tasks, listeners, retries, output, or concurrency; or
- performance claims based only on binary size, source line count, or an
  isolated microbenchmark.

## Target Architecture

The shipped shape is documented in `architecture.md`. This section retains
only the design rules that constrain remaining work.

### Compile Cold, Execute Hot

`compile(profile, resolved configuration, capability snapshots) ->
Arc<CompiledAgentPlan>` runs before the hot path; `run(session, input,
limits, cancellation)` receives direct handles and exhaustive
provider-neutral values. Provider names, addon names, config layering,
manifest discovery, schema compilation, filesystem discovery, secret
resolution, and trust decisions do not belong in the turn loop.

`AgentPlanDescriptor` is the canonical secret-free serializable identity;
`AgentPlanDigest` is the SHA-256 of its canonical encoding, with the encoding
and `DESCRIPTOR_VERSION` pinned by fixtures. Secrets and secret hashes never
enter descriptors, digests, events, traces, snapshots, or cache diagnostics.
Credential rotation uses an opaque `CredentialEpoch` owned by `qq-auth`.

The plan cache has hard entry-count and byte ceilings, evicts LRU inactive
generations, pins active `Arc` generations, and fails compilation with an
explicit capacity error rather than growing. A failed refresh never poisons a
valid generation; active runs keep their admitted generation.

### Ownership Within The Existing Crates

Do not add an agent-framework, plugin, tool-host, context, or addon crate.

| Owner | Responsibility |
| --- | --- |
| `qq-config` | Parse and merge agent-profile and addon declarations, validate syntax, retain provenance |
| `qq-auth` | Resolve provider and addon secret references without exposing secret values to config or protocol |
| Root package | Discover trusted sources, translate external config, compile/cache plans, wire concrete adapters |
| `qq-provider` | Compile provider recipes; one provider-neutral stream handle plus effective capabilities; sole retry owner |
| `qq-core` | `CompiledAgentPlan`, descriptor, execution invariants, context-source contracts, tools, run limits, durable outcomes |
| `qq-mcp` | Bounded MCP capability/catalog snapshots and selected MCP operations |
| `qq-protocol` | Versioned profile IDs, plan digests, input parts, commands, outcomes, events, capabilities |
| `qq-server` | Map authenticated HTTP/SSE onto protocol commands; no agent logic |
| `qq-client` | Bounded command, reconnect, replay, approval, steering, capability APIs |
| `qq-tui` | Project protocol state; never discover or execute addons |

Application configuration types must not leak into `qq-core`.

### Extension Lanes

| Lane | Interface | Load time | Hot-path behavior | Trust/isolation |
| --- | --- | --- | --- | --- |
| Agent packs | Declarative versioned manifest | Discovery/startup | Immutable prompt/profile data | Hash and trust source; no code execution |
| Providers | Rust provider compiler/stream seam | Startup or explicit refresh | Direct provider handle | Compile-time trusted adapter; secrets resolved outside core |
| Native tools | Static Rust registration | Build/startup | Direct dispatch | Fully trusted; capability-scoped execution |
| General tools | MCP and the embedded `ExternalToolHost` | Startup catalog; call on demand | One selected adapter call | MCP process/HTTP boundary or trusted embedder |
| Context/memory | Typed bounded `ContextSource` | Plan compile plus pre-turn fetch | No per-delta hook | Time/byte/token budgets; explicit fail policy |
| Observers | Durable SSE/outbox | Subscription | Post-commit only | Cannot affect authoritative execution |
| Process execution | Local implementation plus one real sandbox adapter (H10) | Startup | Direct selected backend | Explicit filesystem/network/process capabilities |
| Surface adapters | Versioned `qq-client` contract | Client startup | Outside agent loop | Product owns remote auth and UX |

Rules that remain in force: the built-in Rust tools stay the zero-overhead
path and are never wrapped in RPC or a plugin abstraction; the hot path
selects one precompiled tool entry and never runs before/after hook lists;
product memory is not a synchronous observer of every token and ordinary
retrieval fails open with a visible diagnostic; synchronous decisions remain
limited to approval, exact tool validation, and budget admission; provider
adapter families are feature-gated inside `qq-provider` (`provider-bedrock`
owns the AWS SDK closure) rather than split into crates.

### Extension Performance Invariants

- A disabled adapter family adds no shipping dependency to a minimal build.
- Disabled addons add no run-loop allocation and no observer dispatch.
- Plan lookup is digest/cache lookup, not filesystem discovery.
- No observer can block durable commit or client delivery.
- Every extension queue and concurrency permit is bounded.
- A new extension mechanism may not regress the disabled/default hot path by
  more than five percent without an explicitly accepted tradeoff.
- Catalog changes compile a new immutable generation rather than mutating a
  live registry under the run loop; active runs never wait for an unrelated
  addon reload.
- All runtime traces identify the exact plan and addon generations.

## Performance Constitution

### Method

Use fake providers and temporary stores for deterministic runtime latency;
separate provider network latency from QQ latency; use fixed-model live runs
only for outcome and cache qualification. Every phase records the pre-change
baseline for its own new behavior before enforcing a regression gate. Do not
benchmark nonexistent behavior. The reproducible protocol is
[`benchmarks/perf/README.md`](../../benchmarks/perf/README.md); budgets are
`benchmarks/perf/budgets-v1.json`.

The H0 suite (`cargo xtask perf baseline/check`, fixture version 4) covers
binary size and dependency closure, fresh-process startup, server readiness
and shutdown, idle and load RSS, direct and HTTP command acknowledgement,
submit-to-provider-entry, provider delta to committed event and to
authenticated SSE observation, tool dispatch through durable completion,
cancellation, snapshot/reconnect/replay, 1/10/100 concurrent sessions,
subscriber fan-out at 1/8/32, and long-stream scaling at 64 KiB / 512 KiB /
1 MiB. Tail gates on the shared recording host have not been repeatable
(same-binary A/A pairs fail the same gates as A/B pairs with identical
medians); tail acceptance requires a quiet host and failures are retained,
never waived.

### Targets

| Gate | Target |
| --- | ---: |
| Command acknowledgement p95 | `<= 10 ms` |
| Warm claimed run to provider send p95 | `<= 25 ms` |
| Semantic delta to durable commit | `<= 15 ms` p95; `<= 40 ms` p99 |
| Durable delta to TUI | `<= 25 ms` p95; `<= 60 ms` p99 |
| Cancellation | `<= 100 ms` |
| Output starvation with eight active streams | None longer than `50 ms` (executable); `20 ms` carried target owned by H20 |
| One MiB request plus 32 schemas | `<= 10 ms` encode; heap `<= 2x` payload |
| One MiB stream scaling | `<= 2.2x` the half-size work after fixed cost |
| Context overflow sent to a provider | Zero |
| Compaction reduction when required | At least `8x` |
| Stable-prefix provider cache use | At least `80%` where supported |
| Core retry amplification | `< 1.05` provider stream entries per logical turn; transport attempts obey `AttemptPolicy` |
| Release binary / minimal binary | `<= 48,000,000` / `<= 41,000,000` bytes |
| Harness-attributable evaluation failures | `< 0.5%` |

### Trend At Phase Boundaries

Clean detached 100-sample recorder unless noted; medians/p95 as labeled.
Phase 5a values are from the 2026-09-07 recordings on a loaded host and are
candidate medians, retained as the pre-change reference for Phase 6.

| Metric | Phase 0 | Phase 4 | Phase 5 | Phase 5a |
| --- | ---: | ---: | ---: | ---: |
| Submit start to provider entry p95 | — | 13.5 ms | 9.3 ms (median) | — |
| Direct command acknowledgement p95 | 3.5 ms | 3.6 ms | 3.0 / 5.9 ms with 560-event active run | 3.2 / 6.2 ms bimodal tail |
| Eight-stream output service gap p95 | — | 47 ms | 29–45 ms | 23–28 ms |
| 1 MiB / 512 KiB scaling ratio | 2.292x (red) | 1.892x | — | — |
| Cancellation to committed terminal p95 | 9.2 ms | — | 34–48 ms (R4 eight-stream) | 26–32 ms |
| 100-session batch p95 | 7.77 s | 2.42 s | — | — |
| `cursor_replay` (9 events, in-process) median / p95 | — | — | 22.6 / 38.2 µs | 0.87 / 1.7 µs |
| Fan-out delivery to slowest of 32 p95 | — | — | 14.9 ms | ≤ baseline |
| Release binary | 62.60 MB | 66.79 MB | — | 45.48 MB |
| Minimal release binary | — | 54.72 MB | — | 38.73 MB |
| Idle server RSS p95 | 16.06 MiB | 17.82 MB | — | within +1% |
| `plan_for` cold / warm | — | 202 / 7.7 µs | — | 204 / 7.5 µs |
| Compiled plan estimated heap | — | 14.0 KiB | — | — |

Cold `plan_for` is dominated by configuration discovery and credential
resolution, which is why H22 targets the config and auth load paths.

## Open Designs

Designs D1–D4, D6, and D7 shipped in Phase 5 and are described in
`architecture.md`. D1 as written (a `tokio::broadcast` per workspace) was
superseded on 2026-09-07 by a bounded sequence-indexed feed ring; the ring is
the current design authority. The designs below remain to be implemented.

### D5 — Shared Transcript And Precompiled Prompt Prefix (H18)

Problem: `ModelRequest.messages` is an owned `Vec<Message>` cloned per turn;
the system prompt (up to ~128 KiB) is rebuilt and hashed per run; tool
schemas are `serde_json::Value` re-serialized per request; message bytes are
measured twice per run.

Design: `ModelRequest.messages: Arc<Vec<Message>>`, appended with
`Arc::make_mut` (no copy once the previous stream is dropped).
`CompiledAgentPlan` gains `prompt_prefix: Arc<str>` and a cloned SHA-256 state
fed only the per-run suffix, plus `ToolSchemaMeasurement` computed once for
full and static exposure. `ToolSpec.input_schema` becomes a precomputed
`RawValue`; tool-call arguments in history keep their original string. Core
keeps running byte counters updated on push.

Rejected: `Arc<[Message]>` (cannot push); a persistent-vector crate for one
site.

Gates: one MiB request heap ≤2x (from ~3x), encode ≤10 ms, claim to provider
send. Tests: `Arc::strong_count == 1` after stream drop; prefix-plus-suffix
digest equals the full digest (guards persisted `RunPromptIdentity`).
Benchmark before: add `provider_encode` (one MiB plus 32 schemas, counting
allocator) in `qq-provider`; rerun `provider_compiler` and `plan_compile`.

### D8 — Control Admission And Shared Commit (H20, implemented)

Status: implemented 2026-09-09 (`ab6de6f`, `d05e474`); design authority is
[ADR-0011](../adr/0011-shared-commit-across-lanes.md) and
`architecture.md` § Persistence. Retained here only for the acceptance list.

As written, D8 assumed the eight-stream output gap was scheduler wake latency
from thirteen `sleep(1 ms)` overload loops. Deleting the loops and making
runtime-issued store calls wait for a `control_slots` permit
(`Priority::AwaitControl`) was correct and shipped first, but a worker probe
showed the gap is fsync-bound: one commit per output group plus one fsync per
interleaved control write, with the scheduler's claim read cutting almost
every group to one job. The fix is `worker::Joins`: control writes join a
forming group as savepoints and settle on its commit; client reads run alone
and close the group; the scheduler's claim runs after the group without
closing it. Persist-before-publish and control-lane FIFO order are unchanged.

Result (30 interleaved pairs, med / p95): gap 24 / 28 → 20 / 33 ms with 27
of 30 samples at 18–22 ms; completion 284 / 310 → 210 / 228 ms; control
latency 19.7 / 24.2 → 15.9 / 18.4 ms. The median meets the 20 ms target;
the p95 tail is bimodal and not reproduced by a same-binary A/A control.

Remaining acceptance: qualify p95 ≤20 ms on a quiet host, then tighten the
executable budget from 50 ms to 20 ms. The 50 ms cancellation polls in
`qq-mcp`, `hosts/embedded.rs`, and `tools/shell.rs` poll an `Arc<AtomicBool>`
and do not touch the store; converting them to a `Notify` changes the public
`ExternalToolHost::call` signature and moves to H22.

### D9 — Store Identity, Settlement, And Error Consolidation (H21)

Problem: `ClaimedRun` is cloned and fabricated at dozens of sites;
`EventContext` is written as a literal; three settlement paths carry divergent
guards (`complete_run_in_transaction` lacks the `outcome_json IS NULL` guard, a
latent double settle); hundreds of `map_err(|_| Persistence)` sites erase every
SQLite error. H23 fixed the teardown-before-settlement branches explicitly;
H21 must make that ordering structural.

Design: `RunIdentity { workspace_id, session_id, run_id, command_id, kind,
child }` is `Copy` inside `ClaimedRun`; `EventContext::for_run` /
`for_session` replace literals; `RunSettlement { identity, outcome, audit }`
feeds one `settle_run` with the null guard, and the settlement interface makes
successful execution teardown a prerequisite for publishing a terminal event
or releasing session ownership; `PersistenceFault { Sqlite(code), Codec,
Constraint }` rides in `SessionRuntimeError::Persistence` via
`From<rusqlite::Error>`. The remaining `sessions.rs` body (~7.7k non-test
lines plus ~25k test lines; `approvals`, `context`, `execution`, `feed`,
`runtime`, `scheduler`, `store`, `subagents` are already split) then moves
into `sessions/{codec, events, snapshots, transcript, claim, streaming,
tool_calls, settlement, compaction, commands}.rs` with tests under
`sessions/tests/`, as a separate mechanical commit after HC3's behavioral
changes.

Gates: none directly; correctness plus roughly 700 fewer lines. Tests:
settling an already-settled run through the previously unguarded path is a
no-op; every `PersistenceFault` variant is reachable through fault injection.
The HTTP mapping of `Persistence` is unchanged.

### D10 — SSE Framing (H19, conditional)

Problem: the provider `SseDecoder` pushes byte by byte and allocates name and
data strings per event; Anthropic parses each event twice; the ledger clones
ids per argument delta; the client decoder mirrors the per-byte feed.

Design, if measurements justify it: `SseFramer::push(&[u8])` scans for frame
boundaries and yields `SseEventRef<'a> { name, data, id }` over the framer's
buffer; adapters parse `data` once; `ProviderEvent` tool ids become
`Arc<str>`. The client reuses the shape; the framer is duplicated rather than
shared if sharing would add a dependency edge.

Rejected: an event-source crate; parsing to `RawValue` then re-parsing.

Gates: decoder time and allocations on a deterministic local HTTP/SSE
pipeline. The fake-provider stream-scaling ratio does not exercise either
decoder and cannot qualify this. Record the parser baseline first (add
`sse_decode` at 64 KiB / 512 KiB / 1 MiB in `qq-provider`); a documented
no-change decision is acceptable when the benefit is insufficient. Tests:
property test splitting events at every byte boundary; CRLF; multi-line data;
oversized rejection.

### Bundled Fixes (H22)

Cold-path and structural items; none shipped yet except where noted.

- `qq-core`: delete the ~37 `notify(` call sites now redundant with the feed
  (the watch remains as a wake for `subagents.rs`); stale-result pruning by a
  stored tool kind rather than name; move `catalog_blocking` inside
  `spawn_blocking`; hoist `sleep_until` out of three `select!` loops; take
  tool results by value; parse tool arguments once into `RuntimeToolCall`;
  `TurnMode` and `StreamEnd` enums; one bounded UTF-8 read for six copies;
  serialize the descriptor once at compile; gate file-state eviction on a
  counter; borrow when persisting model turns.
- `qq-mcp`, `hosts/embedded.rs`, `tools/shell.rs`: replace the three 50 ms
  cancellation polls of the run's `Arc<AtomicBool>` with a shared `Notify`
  (changes `ExternalToolHost::call`; moved here from H20). `qq-mcp`: release
  the call permit before awaiting the connect mutex. (`ToolSpec` sharing by
  `Arc` shipped in Phase 4.)
- `qq-protocol`: box `SessionSummary` in the summary-carrying event variants
  (wire-neutral); one hash newtype macro for the two identical 32-byte hash
  types; move client body limits into `limits.rs`.
- `qq-provider`: one `StaticHttpAuth` with a per-protocol API-key header
  constant, replacing the `HttpAuth` enum arms and duplicated `build_headers`;
  `RequestAuthorizer` as an enum; body encoding with a capacity hint; skip
  redaction merge when empty; avoid the per-request `HeaderMap` clone; stop
  `Debug`-formatting Bedrock events to count bytes.
- `qq-server` and root: a `COMMAND_ROUTES` table keyed by
  `SessionCommandKind` replacing twelve handlers, asserted equal to the client
  table in a test; `decode_bounded` for the four decode preambles;
  `PlanCache` borrows the key on lookup and stops cloning paths in fingerprint
  checks (generation accounting is H27); the approval reviewer compiles
  through `PlanCache`; headless output leaves the Tokio worker.
- `qq-config` and `qq-auth`: parse each source once per load instead of three
  times; memoize verified ancestor directories; `LazyLock` builtin providers;
  shared lock for credential reads, `chmod` only when the mode differs,
  `resolve_with_epoch` to halve lock cycles. (Explicit pack manifests in
  `probed_paths` and source-evidence capture before reads shipped with H25.)
- `qq-tui`: a `body_mut` that does not drop the session tree index on
  streaming deltas; scan the streaming tail once per frame; compute sidebar
  status only for visible rows; bound `tool_timing` and
  `expanded_tool_calls`; parse key chords once.

### Rejected Or Deferred

- A `SseCodec` trait with a generic `SseProvider<C>`: trait-shaped, no gate.
  Revisit when a fifth SSE adapter forces it.
- Bedrock and Google stream phase enums and the duplicate tool-call tracker:
  fold into the next adapter change.
- Collapsing boxed `stream!` layers: one indirection per event, no
  measurement.
- Template-method tool-call transitions and per-arm command functions in
  `execute_command`: reconsider only if the D9 split exposes real duplication.
- Typed run/tool-call state strings, `RunShared`/`RunState` bundles,
  table-driven budget checks, search/read/hex micro-optimizations: no gate or
  owned by R6.
- A shared `AnswerProjection` across TUI, headless, and `ask` reducers:
  speculative unification.
- Root canonicalize/credential-plan dedup, `Tracked<T>` provenance, a
  `RefreshableCredential` trait: cold path, no gate.
- File splits other than `sessions.rs`: do opportunistically, never as a
  dedicated task.
- A `ValidatedInput` newtype: no bug reported.

## Implementation Sequence

Tasks use `H` identifiers (backend) and `HC` identifiers (headless contract)
to avoid colliding with the readiness plan's `R` numbering. A phase section
follows one template: status line, tasks, acceptance, and a receipt of at most
about fifteen lines with commit SHAs. When a phase closes, its section
collapses to a row in the completed-phases table; full receipts remain in Git
history at the recorded revisions.

### Readiness Dependencies

Milestones owned by `terminal-bench-readiness.md`. R4 and R5 shipped and were
imported in Phase 1.

| Milestone | Owning phase | Required outcome here |
| --- | --- | --- |
| R6 | Tool tournament and terminal | Any richer search/edit/terminal contract has won its ablation and cleanup gates; gates H10 |
| R7 | Sub-agent economics and scheduling | Child accounting/admission behavior is measured and remains bounded |
| R8 | Warm runtime and request efficiency | Credential-lease caching, MCP bounds, provider prompt-cache determinism. Retry exposure, shared message storage, and request-encoding benchmarks were handed to H14 (shipped) and H18 |

### Task Index

| Task | Status | Outcome | Depends on | Owner |
| --- | --- | --- | --- | --- |
| H0 | Done | Speed, size, RSS, replay, and concurrency baseline; `xtask perf` | — | `xtask` |
| H1 | Done | `provider-bedrock` feature; full/minimal profiles and budgets | H0 | Root, `qq-provider` |
| H2 | Done | `CompiledAgentPlan`, secret-free descriptor, root `PlanCache` | H1, R5 | Root, config, core, provider |
| H3 | Done | Input parts, profiles, plan identity, limits, steering, capabilities, correlation | H2 | Protocol, server, client |
| H4 | Fixtures done | External SDK deferred to a real consumer | H3 | `qq-client` |
| H5 | Done | `pack.ron` agent packs compiled into plans | H2 | Root, config, core |
| H6 | Done | Immutable `ToolCatalog`, progressive disclosure, `select_tools` | H2 | Root, core, MCP, protocol |
| H7 | Done | `ExternalToolHost`, `EmbeddedToolHost`, shared conformance suite | H6 | Core, MCP, root |
| H8 | Done | Bounded `ContextSource` with cache and fail policy | H2 | Core, root, protocol |
| H9 | Done | `qq-client::observer` post-commit loop | H3 | Protocol, client, server |
| H13 | Done | Effect-classified approval; `ToolClass::Unknown` deleted (D4) | H6, H7 | `qq-core` |
| H14 | Done | Provider is the single retry owner; `AttemptPolicy` (D3) | H2 | `qq-provider`, `qq-core` |
| H15 | Done | Published-event outbox and workspace feed (D1, superseded by the ring) | H9 | `qq-core`, `qq-server` |
| H16 | Done | Output-lane group commit, savepoints, statement cache (D2) | H15 | `qq-core` store |
| H17 | Done | Schema 25: `runs.activity`, command counter, two-hop claim (D6, D7) | H16 | `qq-core` store |
| H23 | Done on Linux | Supervised-child ownership across admission, overload, steering, cleanup | Delegation D4 | `qq-core` |
| H24 | Done | Fresh child/audit admission, finite-spend serialization, descendant receipts | Delegation D2 | `qq-core` |
| H25 | Done | Live provider/MCP credential binding invalidation without secret-bearing identity | H2, H7 | Root, auth, MCP |
| H26 | Done | Bounded workspace-feed admission and lifecycle; feed ring | H15 | `qq-core`, server |
| HC2 | Done | Positive tool exposure via optional `policy.exposed_tools` | H6, H13 | Config, core plan |
| H20 | In review | Lifecycle store calls wait for admission (13 loops deleted); control writes share the output group commit; scheduler claim no longer closes groups (D8, ADR-0011). Gap median 20 ms; p95 qualification open | H16, H23–H26 | `qq-core` |
| **H21** | **Next** | `RunIdentity`, `PersistenceFault`, one settlement path; then the `sessions.rs` split (D9) | H15–H17, H20 | `qq-core` |
| H27 | Partly open | Same-key refresh drops the old generation before admission; superseded active generations are absent from byte accounting; a rejected replacement loses the old entry; equivalent-plan refresh can grow source evidence without admission; completed per-key compile guards are retained. Pinned-generation LRU and admission already exist and are tested | H2 | Root |
| H28 | Open | A ninth context source is silently ignored (`lib.rs`); source identity, version, budget, and fail policy are absent from the descriptor | H8 | Core, protocol |
| H22 | Open | Bundled cold-path and structural fixes (list above) | — | Per crate |
| H18 | Open | Shared transcript `Arc`, precompiled prompt prefix, `RawValue` schemas (D5) | H14 | `qq-core`, `qq-provider` |
| H19 | Conditional | SSE framing (D10) only if the decoder baseline justifies it | H18, `sse_decode` baseline | `qq-provider`, `qq-client` |
| HC1 | Open | `--correlation`, exclusive `--session` resume, `u32` turn limits, model-less `config check` | H3, H26 | Root, config, core, protocol |
| HC3 | Open | `--output-schema`, bounded repair turns, `final_output` on `outcome`/`RunFinished` | H3, HC1 | Protocol, core, root |
| HC4 | Open | Headless golden fixtures per `PROTOCOL_VERSION` and compatibility statement | HC1–HC3 | Protocol tests, docs |
| H10 | Gated | First real OS process-sandbox adapter | R6, platform threat model | Core tools, root |
| H11 | Gated | Optional ACP/OpenAI compatibility facade | H4, real consumer | Existing surface owner |
| H12 | Gated | Crash, load, security, quality, and performance qualification | All shipped tasks and required R milestones | Workspace-wide |

### Completed Phases

| Phase | Tasks | Closed | Revision | Landed |
| --- | --- | --- | --- | --- |
| 0 — Speed constitution | H0 | 2026-09-01 | `6383305` | `cargo xtask perf baseline/check`, versioned JSON reports, `budgets-v1.json`, deterministic fake-provider fixture with 1/10/100-session load and RSS sampling |
| 1 — Prerequisites and profiles | R4, R5, H1 | 2026-09-02 | `5bb1471` | Linear/fair streaming and resolved model, context admission, `RunLimits`, and compaction hardening imported from the readiness plan; `provider-bedrock` gates the AWS crates |
| 2 — Compiled plan | H2 | 2026-09-02 | `2d2ba3b` | `AgentProfile`, secret-free `AgentPlanDescriptor` with canonical digest, runtime-only `CompiledAgentPlan`, `SourceFingerprint` revalidation, `CredentialEpoch`, root `PlanCache` (16 entries / 64 MiB, LRU, pinned generations, single-flight) |
| 3 — Backend contract | H3, H4 fixtures | 2026-09-03 | `dfaebb9` | Protocol 13, schema 21: `InputPart`, `Correlation`, `AgentProfileId`, `RunPlanIdentity`, `SteerRun`, `SetSessionProfile`, expanded `RunLimits`, `ServerCapabilities`, config `profiles`, 24 golden fixtures |
| 4 — Extensions | H5–H9 | 2026-09-03 | `f02cfc9` | Protocol 14: immutable `ToolCatalog` with progressive exposure; `ExternalToolHost` with `EmbeddedToolHost` and a shared conformance suite; `pack.ron` packs; bounded `ContextSource`; `qq-client::observer`; `ToolSpec` behind `Arc` |
| 5 — Correct the hot path | H13–H17 | 2026-09-04 | `ea5a6af`…`70166bd` | Effect-classified approval; provider-owned retry (descriptor 4→5); published-event outbox; output-lane group commit with a 128-statement cache; schema 25 with `runs.activity`, command counter, grouped snapshot accounting, two-hop claim, joined context assembly. Amplification measured 1.000; fan-out to slowest of 32 26.4→14.9 ms; `store_output_batch` 236→138 ms. The ≤20 ms service-gap gate was **not met** (29–45 ms) and is carried to H20 |
| 5a — Repair shipped contracts | H23–H26 | 2026-09-07 (tail acceptance open) | `1e6a901`, `f482b37`, `893e582` | H23 child ownership across admission/overload/steering/cleanup with fail-closed teardown; H24 per-admission child budgets, deadline carry, owned-descendant spend, `child_admission` bench; H25 exact redacted live credential bindings separate from durable identity, eager MCP after admission, pre-read source evidence; H26 validate-then-attach feeds with lease reclamation (retained RSS after 4096 rejected subscribes 135 MB→0), then the sequence-indexed feed ring (`cursor_replay` 22.6→0.87 µs median, 2001→1 store reads) and release profile (`strip`, `codegen-units = 1`, thin LTO; minimal binary −30.5%, default −33.0%; budgets tightened to 41/48 MB). Focused R4/shell/cache comparisons pass their gates; full H0 tail gates are not repeatable on the shared host (A/A fails the same set) and remain retained, not waived. Native Windows teardown runs as a targeted CI job (`windows-teardown`); full native qualification is not claimed |

Retained decisions from those receipts:

- Persist-before-publish, one terminal event per run, idempotent commands,
  and cursor replay are the invariants every later phase must keep.
- The warm plan path performs no filesystem discovery beyond the recorded
  `stat` list; refresh failures never poison a valid generation; active runs
  keep their admitted generation; equivalent plans with changed live bindings
  compile new bindings without disturbing active handles.
- Static built-in tools never enter the exclusion or pin paths.
- A durable terminal event is not proof of execution teardown; settlement
  requires drained children, blocking file operations, and owned shell tasks,
  and unconfirmed teardown fails the runtime closed.
- Tail-only budget failures that reproduce in a same-binary control are
  recorded as non-repeatable, not accepted as regressions and not waived.
- The `feed_attach_replay` focused fixture subscribes from the initial cursor
  and measures only the cold path; correcting it is follow-up work under H22.

### Phase 5a — Remaining Acceptance

Status: implemented; tail qualification open. No further code is scheduled.

Open items, tracked here until closed:

- Repeat the full version-4 H0 baseline/candidate comparison on a quiet host
  (I/O pressure `some avg10` well under 20%). Baseline `1c08cef` (pre-H23 `main`), candidate
  `main`. Record the result as one line in the Completed Phases row.
- Native Windows teardown: the targeted CI job passes; a full Windows
  workspace run has not been executed and is not claimed.

### Phase 5b — Headless Contract For Supervisors

Status: HC2 shipped 2026-09-06 (`893e582`, squashed from `93ef6b8`); HC1, HC3, HC4 open. Design
authority, gap table, bounds, and acceptance criteria are in
[`headless-contract.md`](../design/headless-contract.md); this section records
only sequencing and the constraints that interact with Phase 6.

- HC1 and HC3 are independent of Phase 6 except that HC3's settlement and
  prompt-identity changes must land before the mechanical H21 `sessions.rs`
  split and coordinate with H18's prompt prefix and H28's descriptor change.
  HC1's `u16→u32` turn-limit widening and HC3's `final_output` each need a
  `PROTOCOL_VERSION` bump and fixtures; they may share one bump only if they
  integrate atomically.
- HC4 lands last and pins the whole under
  `crates/qq-protocol/tests/fixtures/headless/v<PROTOCOL_VERSION>/`.
- Boundary rules: no supervisor-only mode or product vocabulary; QQ acquires
  no new authority; new JSONL fields are additive and optional; the default
  `qq run` payload is preserved after normalization.
- HC3 records enabled schema-compilation, validation, and repair measurements
  before acceptance; the cold-path classification does not exempt opt-in
  runtime work from performance and resource acceptance.
- HC2 receipt: optional `policy.exposed_tools` intersects across layers and
  with profile/pack exposure before trust-sensitive grants; static names and
  MCP name syntax validate in `config check`, MCP membership at plan
  compilation; grants cannot restore an excluded tool. Workspace suite 1207
  passed; combined default-path H0 qualification rides with Phase 5a.

Phase 5b is complete when HC1–HC4 acceptance fixtures are green, every gap
row in `headless-contract.md` reads Shipped with its commit, and the
workspace gates and default-path H0 regression gate pass.

### Phase 6 — Finish Fairness, Shrink Per-Run Work, And Consolidate

Status: active from 2026-09-08. H20 implemented 2026-09-09 (`ab6de6f`,
`d05e474`; ADR-0011), in review with its p95 qualification open. Order from
here: behavioral H21 (settlement interface), H27, H28, and the correctness
items of H22 (`notify` deletion, stored-kind pruning, MCP permit ordering);
then H18; then H19 after its decoder baseline; then the mechanical H21 split
and remaining structural H22 items as separate commits.

Benchmarks to record before each change:

- H20: cancellation under 256 queued control jobs and the eight-stream mixed
  control/output fixture, with queue admission/dequeue/commit timing (the
  pre-change attribution baseline recorded in `893e582` is the reference);
- H18: `provider_encode` (one MiB plus 32 schemas, counting allocator) in
  `qq-provider`, plus reruns of `provider_compiler`, `plan_compile`, and warm
  `plan_for`;
- H19: `sse_decode` at 64 KiB / 512 KiB / 1 MiB in `qq-provider`, plus
  allocations and latency through a deterministic local HTTP/SSE pipeline;
- H22: the H0 cold `plan_for` measurement and the TUI 200-session sidebar
  case.

Acceptance:

- cancellation is at most 100 ms with 256 queued control jobs and no site
  polls the store (met: 23 / 27 ms med / p95; zero `sleep(1 ms)` loops);
- the eight-stream output service gap is at most 20 ms under mixed
  control/output load with no relaxation of cancellation or durability
  (median met at 20 ms; p95 33 ms with a non-repeatable tail — qualify on a
  quiet host, then tighten the executable budget from 50 ms to 20 ms);
- settling an already-settled run through any path is a no-op; every
  `PersistenceFault` variant is reachable in tests; successful teardown is a
  structural prerequisite of terminal publication;
- active and superseded plan generations obey entry/byte limits; a rejected
  refresh leaves the previous generation intact, including an equivalent-plan
  refresh whose source evidence grows; completed per-key compile guards are
  reclaimed under distinct-key churn without admitting concurrent same-key
  compiles; explicit configuration in request keys is compared privately,
  redacted in diagnostics, and never hashed;
- excess required context sources fail compilation with a typed capacity
  error before provider work; changing source identity, version, budget, or
  fail policy changes the plan digest (`DESCRIPTOR_VERSION` bump with fixture
  re-pin);
- one MiB request heap is at most 2x the payload and encode is at most
  10 ms; the prefix-plus-suffix prompt digest equals the full digest; the
  1 MiB / 512 KiB ratio stays at or below 2.2x and improves on 1.892x;
- if H19 ships, it improves decoder-specific allocation/latency; a documented
  no-change decision is acceptable;
- the `sessions.rs` split changes no behavior and lands as its own commit;
- the H22 route-table equality test passes between client and server; and
- the default path stays within the regression gate for every H0 metric
  against the Phase 5a reference.

### Phase 7 — Execution Quality And Isolation

Implement H10 only after R6 has selected and shipped a real terminal/process
contract and a platform threat model defines the isolation boundary. This
document does not redefine the search, edit, terminal, sub-agent, scheduling,
or warm-runtime contracts.

Acceptance: the readiness plan records completion evidence for each R6–R8
milestone a shipped extension requires; sandbox tests prove filesystem,
network, process, and secret boundaries; local and sandbox adapters pass one
shared process contract suite; sandbox failure never silently falls back to
unsandboxed execution; terminal/process cancellation, timeout, shutdown, and
recovery leak no processes; the sandbox adapter is optional and feature-gated
where its platform dependencies are not needed.

### Phase 8 — Product Adapters On Demand

Implement H11 only for an actual client (ACP, OpenAI-compatible HTTP,
messaging gateway, cron, voice, browser/desktop, or a product memory service).

Acceptance: the adapter uses `qq-client` or the native HTTP protocol;
introduces no alternate runtime or direct store access; native QQ events
remain authoritative; capability loss in the compatibility protocol is
documented and tested; product auth and tenancy remain outside `qq-core`; the
adapter can be disabled without affecting the base binary's hot path.

### Phase 9 — Qualification

H12 qualifies the complete story. Each preceding repair already carries its
own fault, cancellation, and recovery fixtures; H12 is the combined-system
qualification. It re-runs every Phase 5–6 pre-change baseline and enforces the
recorded improvements as regression gates in `budgets-v1.json` or a successor.

Required scenarios: cold and warm direct/TUI/server execution; 1/10/100
concurrent sessions; long text and reasoning streams; provider failure before
send and ambiguous failure after send; store saturation, disk-full,
corruption, migration, and restart; client disconnect/reconnect during text,
tool, approval, and terminal work; addon discovery failure, refresh failure,
crash, timeout, overload, and shutdown; context-source slowness and stale
cache; MCP and embedded tool conformance; cancellation at every
provider/tool/sub-agent boundary; terminal process cleanup; sandbox escape
attempts; same-model agent-quality comparison; minimal/full binary, RSS,
startup, and latency gates.

## Verification Strategy

Unit and contract tests: provider request/stream fixtures for every protocol
and delivery state; plan-digest and invalidation fixtures; manifest parsing,
precedence, provenance, trust, and capability tests; tool-host and
context-source conformance; exhaustive run-limit and terminal-outcome tests;
protocol compatibility and unknown-field tests; queue, output, concurrency,
and cancellation bounds.

Durable integration tests with fake providers, temporary stores, and
temporary workspaces prove: commands are idempotent; events publish only after
commit; accepted runs have one terminal event; restart does not repeat
possibly-executed tools; plans and profiles remain stable across config
changes; child ownership and authority survive restart; external tool/context
failures settle deterministically; cursor replay reconstructs the same client
state.

Performance tests that exist: `provider_compiler`, `plan_compile`/`plan_for`,
`store_output_batch`, `child_admission`, the H0 suite (fan-out, replay,
cancellation, load, RSS, size), the R4 fairness matrix, and the
`provider_retry_amplification_milli` counter. To add: `provider_encode`
(H18), `sse_decode` (H19), cancellation under 256 queued control jobs (H20),
static/MCP/embedded tool-dispatch comparison, context-source cold/warm,
persistent terminal (R6), TUI render.

Quality evaluation: same-model, same-prompt paired evaluations for tool,
context, delegation, and compaction changes, recording verified success,
wall-clock, turns and tool calls, tokens by class, estimated cost, harness
failures, overflow/compaction events, and task-specific correctness. Do not
accept a microbenchmark win that reduces verified success or moves work into
additional model turns.

Workspace gates for every implementation phase:

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace
cargo test -p qq-provider --no-default-features --features test-support
```

## Risk Register

| Risk | Failure mode | Mitigation |
| --- | --- | --- |
| Universal plugin abstraction | Every run pays dynamic discovery/hook cost | Separate deep extension lanes and immutable plans |
| Addon trust confusion | Declarative package gains accidental code authority | Manifests contain data; executable behavior uses explicit native/MCP/embed boundaries |
| Cache invalidation | Run uses stale prompt/tool/policy or stale live binding | Digest every behavior-affecting input; exact redacted live bindings (H25); H27 accounting |
| Reload races | Active run observes partially updated catalog | Compile new generation off-path and atomically swap for later runs |
| Provider feature flags | Full and minimal behavior diverge | Shared contract fixtures and the minimal-profile test in the workspace gates |
| External tool overload | Runtime tasks or output grow without bound | Per-host and global permits, deadlines, quotas, bounded queues/output |
| Context latency | Retrieval dominates TTFT | Cache/prefetch, strict budgets, explicit fail policy, trace separately |
| Observer coupling | Analytics/memory delays output | Consume committed events asynchronously with cursors |
| Retry ambiguity | Duplicate billed request or side effect | Provider restarts only before the first yielded event; delivery certainty |
| Compatibility facade | Lowest-common-denominator API becomes architecture | Native QQ protocol remains authoritative |
| Terminal complexity | Leaked processes or platform divergence | One-shot shell stays fast; durable supervisor is bounded and qualified |
| Sandbox claims | Policy UX mistaken for isolation | Call it a sandbox only after adversarial platform tests pass |
| Scope expansion | Backend absorbs Hermes product features | Keep product adapters above `qq-client` |
| Benchmark gaming | Startup/size improves while outcomes regress | Measure useful-result latency, reliability, cost, and resource use together |
| Tail noise | Non-repeatable tail failures are waived or misread as regressions | Same-binary A/A control on every recording; failures retained until a quiet-host run |
| Prompt prefix memoization | Prefix-plus-suffix digest diverges from the full digest | Digest-equality fixture is a Phase 6 acceptance gate |
| Consolidation churn | Mechanical splits hide behavior changes | D9 lands behavior and the file split as separate commits |

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

- full and minimal builds have enforced binary, startup, RSS, and latency
  budgets, and tail gates have been accepted on a quiet host;
- streaming and reasoning persistence meet linearity and fairness gates,
  including the 20 ms eight-stream service gap;
- effective model capabilities and core-owned run limits are durable and
  visible through every interface;
- the native protocol supports structured input, profiles, steering,
  capabilities, replay, typed outcomes, and typed final output;
- a real external product can use QQ through a thin client without spawning a
  fresh process per turn or importing QQ internals;
- `CompiledAgentPlan` removes discovery/configuration work from warm runs and
  records reproducible plan identity including context sources;
- unused provider families and addon mechanisms impose no minimal-build or
  default-hot-path cost beyond accepted gates;
- agent packs, MCP, the embedded tool host, context sources, and the
  post-commit observer pass conformance and failure tests;
- every queue, task, process, retry, output, and concurrency dimension is
  bounded, and retry has exactly one owner;
- event delivery serializes each committed event once and reads the store
  only for cold cursors; output persistence commits in bounded groups;
- every tool call is approval-classified from catalog effect data;
- settlement structurally requires execution teardown, and every persistence
  failure retains its source;
- terminal and sandbox behavior, if shipped, passes cleanup and adversarial
  tests on supported platforms;
- crash/restart never repeats uncertain side effects and every accepted run
  settles durably;
- same-model evaluations show that shipped tools, context, and delegation
  improve verified work per dollar and minute; and
- product integrations remain clients of one durable QQ runtime.
