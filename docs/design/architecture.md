# QQ Architecture

## Purpose

QQ is a local-first agent harness for querying LLMs and using them to inspect,
modify, build, and test software. It will support interactive terminal use,
non-interactive automation, and long-running remote sessions without splitting
those use cases into separate products.

The architecture is ordered by two product priorities:

1. Speed and resource efficiency.
2. Developer friendliness.

Correctness, durability, and safe tool execution are baseline constraints. A
faster system that loses history or corrupts a workspace is not useful.

## Initial System Shape

QQ ships as one Rust binary named `qq`.

```text
TUI / CLI client
       |
       | HTTP commands + SSE events
       v
QQ server
  |-- agent runtime
  |-- model client
  |-- tool executor
  `-- SQLite store
```

The binary has multiple process modes:

- `qq` opens the TUI scoped to the current working directory. By default it
  starts a local server runtime in the same process and communicates with it
  through the same HTTP/SSE interface used by remote clients.
- `qq serve [ARGS]` runs the server without a TUI. It is suitable for a
  persistent process on a desktop or home server.
- `qq ask PROMPT` is the initial direct, automation-oriented path. It streams
  one model response to stdout through the same core runtime that the server
  will use.
- Additional direct CLI commands must reuse the same runtime rather than create
  another agent implementation.

Keeping the TUI and server in one executable provides a zero-setup local path
while still allowing several TUI or future browser clients to attach to a
long-running server. Agents and sessions belong to the server, so they may
continue when a client disconnects.

An embedded or standalone server must reserve the user-scoped instance lock
before opening `SessionRuntime`. Runtime construction performs crash recovery
and starts scheduling immediately; constructing it before ownership is known
would let a losing startup race mutate or claim work from the winning server's
store. Dropping an unstarted reservation removes its metadata and releases the
lock.

## Repository Layout

QQ is a Cargo workspace whose root package builds the `qq` binary. Library
crates live under `crates/`, while repository automation lives in `xtask/`.

The workspace is:

```text
Cargo.toml
src/
  main.rs
  cli.rs
  catalog.rs
  mcp.rs
  output.rs
  runtime.rs
crates/
  qq-auth/
    Cargo.toml
    src/lib.rs
  qq-client/
    Cargo.toml
    src/lib.rs
  qq-config/
    Cargo.toml
    src/lib.rs
  qq-core/
    Cargo.toml
    src/lib.rs
  qq-mcp/
    Cargo.toml
    src/lib.rs
  qq-provider/
    Cargo.toml
    src/
      lib.rs
      model.rs
      compiler.rs
      construction.rs
      providers.rs
      providers/
        openai.rs
        openai_chat.rs
        anthropic.rs
        google.rs
        bedrock.rs
        mantle.rs
        support.rs
  qq-protocol/
    Cargo.toml
    src/lib.rs
  qq-reasoning/
    Cargo.toml
    src/lib.rs
  qq-server/
    Cargo.toml
    src/lib.rs
  qq-tui/
    Cargo.toml
    src/lib.rs
xtask/
  Cargo.toml
  src/
    main.rs
    providers.rs
```

- The root `qq` package is the executable and composition root. It owns process
  startup, top-level CLI dispatch, runtime construction, authenticated model
  discovery, and translation between crate-specific settings. It contains no
  provider, HTTP/SSE, credential-store, or configuration-file implementation.
- `qq-auth` contains provider-side OAuth flows, credential storage, keyring
  integration, and resolution of provider-neutral secret references.
- `qq-client` contains the authenticated HTTP/SSE client, bounded decoding,
  reconnect/replay behavior, and the session client port used by the TUI.
- `qq-config` contains layered configuration, built-in provider/model presets,
  managed policy, remote organization documents, and config provenance. It
  returns config-owned TUI values; the root translates them into `qq-tui`
  settings.
- `qq-core` contains the agent loop, session behavior, tool integration, and
  persistence behavior. It consumes the command and event vocabulary from
  `qq-protocol` and exposes a small interface that hides orchestration details
  from clients.
- `qq-provider` contains the provider-neutral model interface and concrete
  model-provider adapters. `lib.rs`, `model.rs`, and `compiler.rs` form its
  public facade; concrete adapters live privately under `providers/`. It also
  owns the provider-neutral secret-reference vocabulary shared by config and
  auth. The Amazon Bedrock family is the one feature-gated adapter
  (`provider-bedrock`, default on) because it alone carries the AWS SDK
  closure; recipes and neutral types compile in every profile and the
  compiler refuses that family with a configuration error when it is absent.
- `qq-protocol` contains shared identifiers, commands, events, and versioned
  wire types, plus the redacted local-server connection capability shared by
  the server and client adapters. It does not depend on an HTTP client or
  server framework.
- `qq-server` contains the Axum adapter, HTTP/SSE route wiring, bearer-token
  authentication, and private local-instance discovery metadata.
- `qq-tui` contains terminal rendering, input handling, and client-side state.
  It communicates through `qq-client` and the protocol and does not depend
  directly on `qq-core` or application configuration. Rendering is retained:
  one `TranscriptCache` holds laid-out messages keyed by width for the shown
  session, streaming messages lay out only their open block,
  syntax highlighting runs off the render tick, and frames are diffed by row
  against the previous frame with hand-rolled style primitives rather than a
  widget framework. One command registry drives keybindings, slash commands,
  and pickers; colors come from a resolved theme the root passes in.
- `xtask` contains repository maintenance tasks and is not shipped as part of
  QQ.

The direct workspace dependency graph is:

```text
qq (composition root)
qq-server    -> qq-core, qq-protocol
qq-tui       -> qq-client, qq-protocol
qq-client    -> qq-protocol
qq-config    -> qq-provider
qq-auth      -> qq-provider
qq-core      -> qq-provider, qq-protocol
qq-mcp       -> qq-provider
qq-provider  -> qq-reasoning
qq-protocol  -> qq-reasoning
```

Dependencies point toward `qq-protocol` and `qq-provider`; `qq-client` and
`qq-server` do not depend on one another, and neither `qq-config` nor `qq-auth`
depends on the other. The root package wires them together. Application
configuration types must not become a shared dependency imported throughout
the workspace; the root translates external configuration into each crate's
settings.

Do not create additional placeholder crates for storage, tools, individual
providers, plugins, web, or mobile. A module should become a crate only when a
measured build concern or multiple real consumers justify the seam.

## Runtime

QQ uses stable Rust with Tokio as its async runtime and Clap for command-line
parsing. Dependencies are added only for implemented behavior. Prefer a small,
well-understood dependency over a framework or abstraction stack.

The server owns session state and schedules work using bounded Tokio tasks and
channels. Every long-running operation must support cancellation. Model calls,
tool output, persistence, and client delivery must apply backpressure rather
than create unbounded queues.

## Provider Compilation

Provider names are configuration presets, not runtime dispatch keys. The root
package translates layered configuration into a `qq-provider` recipe, and the
provider compiler validates that recipe before returning the single
`Provider::stream` interface consumed by `qq-core`. Concrete adapter modules and
constructors are crate-private; downstream crates cannot bypass compilation.

```text
provider configuration
        |
        v
typed provider recipe
        |
        v
ProviderCompiler -- shared HTTP pool
        |
        v
configured Provider::stream
```

A recipe separates deployment identity from its wire protocol, endpoint mode,
and authentication intent. Built-in and custom deployments compile through the
same path. Base endpoints append protocol path segments; exact endpoints are
never rewritten. Invalid protocol/authentication combinations fail during
compilation rather than during a model request.

`construction.rs` owns the one compatibility matrix that resolves public
`HttpAuth` intent into protocol-specific headers and request authorization. The
internal Mantle authorizer cannot be expressed by a public recipe. Base versus
exact endpoint intent has one representation beside `EndpointSpec`.

Provider compilation follows these performance rules:

- One `ProviderCompiler` and HTTP connection pool are shared by every model in
  a runtime factory.
- Provider configuration, URLs, headers, and protocol choices are validated
  once and remain immutable while streaming.
- Shared providers pass directly into `qq-core`; they are not boxed and then
  wrapped again.
- Immutable model identifiers use shared storage so each command does not
  allocate another model string.
- Provider identity must not cause branching in the request hot path.

For each run, the root composition layer also projects effective configuration
and model metadata into one immutable, secret-free `ResolvedModel`. The value
records the effective route, provider-visible model, output cap, optional
context window, pricing provenance, named organization/credential profile, and
implemented generation/cache controls without leaking configuration or secret
types into `qq-core`. Core verifies that the runtime's model and output cap
match the descriptor, persists the descriptor once on the run, and only then
permits provider polling. Per-turn audit rows retain the effective model
selection and refer back to the run instead of copying the full descriptor.
Historical rows remain explicitly unknown rather than being reinterpreted
through current configuration.

Version-2 resolved models also carry an optional opaque provider request-shape
identity built once by the root from the effective adapter, API, endpoint mode,
safe normalized endpoint, explicit region, and non-secret authorization shape.
An AWS provider-chain region that can change across restart stays unknown. The
root rejects Custom and LiteLLM endpoint provenance before hashing any URL
bytes because an arbitrary path may carry credentials. Built-in deployments
also stay unknown when endpoint userinfo, query, fragment, or custom static
headers prevent a secret-free identity. Core combines a
known provider identity with the provider-visible model, organization,
generation/cache/output controls, and exact system/tool prefix. After a
measured prompt turn, it persists that versioned basis, request byte count, and
`context_tokens` atomically in the existing model-turn transaction. The next
run's existing reservation query loads the basis without another store call.
Only an exact shape/prefix match with monotonically growing request bytes may
seed the conservative context estimate. Pricing-only refreshes are compatible;
missing usage, model changes, successful compaction, malformed/unsupported or
assembly-rewritten history, or any wire/prefix mismatch clear or disable reuse.
Provider-overflow suppression is deliberately weaker than reuse: when the
provider identity is unknown (Custom/LiteLLM, dynamic AWS region chains,
historical descriptors), core falls back to a route-level shape in a separate
digest domain, and pruned history does not discard the evidence. A repeated
shape and static prefix therefore always compacts once before polling again,
while an unknown identity never persists an occupancy basis for reuse.
Schema version 18 stores that overflow basis in its own additive column; the
version-17 resolved-model overflow column remains legacy state and is ignored
because it lacks the static-prefix identity and measured request byte count.

### Compiled Agent Plans

Everything about an agent's behavior that does not depend on the prompt is
compiled once into an immutable, runtime-only `CompiledAgentPlan` owned by
`qq-core::plan`: the compiled provider handle, the `ResolvedModel`, the opened
capability-scoped workspace with its instruction file already read, the
compiled `ToolCatalog` (built-ins, the `spawn_agent`, `search_history`,
`select_tools`, and `load_skill` declarations, and every admitted external tool
with its serialized schema and digest), the compiled `SkillIndex`, the
sub-agent routes, the external tool hosts, the selected agent pack, and the
context sources. Durable session runs execute directly
from the plan and perform no canonicalization, directory open, instruction
read, or host catalog request before the first provider request; only an
explicitly invoked command or skill document is still read per run. External
tool declarations are frozen into the plan at compile time as a catalog
generation: a run keeps the catalog it was admitted with, and a host that
changes its catalog (an MCP `list_changed`, a reconnect, a shutdown) makes the
plan stale so the next load recompiles.

The root builds a typed `AgentProfile` from configuration and compiles it; core
never sees configuration documents, secret values, or the credential store. The
plan's `AgentPlanDescriptor` is its secret-free canonical account: adapter
build identity, provider `id`/API/sanitized endpoint/auth scheme/credential
*reference* (environment name, stored name, profile, inline, ambient chain)
and static header *names*, the resolved model, workspace root, prompt version,
instruction hash and source, the tool catalog (digest, exposure, admitted
names, host generations, typed exclusions), the skill index, the selected pack
(identifier, version, manifest digest, persona hash, tool policy), spawn
routes, configuration grants, MCP server declarations, and configuration
source labels. Retry is the provider's alone (`qq_provider::AttemptPolicy`)
and is not part of the plan. `AgentPlanDigest` is the SHA-256 of a domain-tagged compact JSON
encoding in declaration order (`DESCRIPTOR_VERSION` pins the encoding). Secret
values, secret hashes, live handles, and the credential epoch never enter the
descriptor or its digest.

Credential rotation is tracked separately by an opaque `CredentialEpoch` owned
by `qq-auth`: every durable credential write advances the store's index
revision, including in-place rotation of an existing entry. The root records
the epoch beside a compiled plan. Live provider access and admitted MCP
declarations also retain exact in-memory equality, including inline credentials,
header values, and full endpoints. These live bindings are never hashed or
serialized into durable identity. An epoch or binding change rebuilds live
authorization while active runs retain their original handles.

The root's `PlanCache` holds one generation per (canonical workspace, model
selection, explicit configuration) key and revalidates it on every load with a
fixed list of `stat` calls: every path the configuration loader probed
(`ConfigSnapshot::sources`, captured before discovery and reads), the credential index file, the workspace's
`AGENTS.md`/`CLAUDE.md`, the skill roots the index was compiled from, and the
selected pack's manifest and persona, plus one in-memory generation compare
per external tool host. A warm lookup performs no configuration parsing,
credential I/O, directory listing, or host round trip. Any observable change recompiles; an
identical digest, epoch, and live bindings keep the live generation, otherwise the new
generation is published atomically for later runs while active runs keep the
`Arc` they were admitted with. A failed recompile returns the configuration
error to the triggering run and leaves the previous generation cached. The
cache has hard entry and estimated-byte bounds, evicts least-recently-used
inactive generations, never evicts a generation an active run holds, fails
admission explicitly when pinned generations exhaust the bound, compiles one
generation per key at a time under refresh storms, and refuses loads after
shutdown.

A run's `RunPlanIdentity` — the selected profile, descriptor version, digest,
and credential epoch — is written in the same statement that moves the run to
`running`, beside the resolved model and the canonical descriptor JSON. It is
carried on `run_started` and `RunSnapshot.plan`. A later refresh never touches
that row.

### Agent Profiles

An `AgentProfileId` names a bundle of per-session defaults declared in the
configuration's `profiles` map: model route, organization, output cap, and
approval mode. `default` is implicit and cannot be declared. `qq-config`
validates names and routes at merge time exactly like the top-level model;
the root resolves a profile when compiling a plan (explicit session selection
wins over the profile, which wins over the top-level configuration) and keys
the plan cache by profile, so two profiles that happen to resolve identically
are still distinct plans because the caller selected them by name. The
profile is part of the descriptor and therefore the digest. A profile the
configuration no longer declares fails the run that needs it with a
`configuration` failure; `POST /v1/capabilities` lists what a workspace
declares. `qq-config` does not depend on `qq-protocol`; the root translates.

### Tool Catalog And External Hosts

The catalog is compiled once per plan by `qq-core::catalog` from the static
built-ins and every `ExternalToolHost` the root attached. Static tools are
trusted; profile/pack policy and optional `policy.exposed_tools` may remove
them before catalog construction. Exposure lists intersect across config
layers, and grant no execution authority. External tools are validated by name shape
(`mcp__<server>__<tool>`, `ext__<host>__<tool>`), deduplicated against the
static names and each other, bounded per tool (16 KiB schema, 4 KiB
description) and per catalog (512 tools, 1 MiB of external schema), and every
refusal is recorded as a typed `ExcludedTool` in the descriptor and the
capability document so the rest of the host stays usable. Entries are sorted
by name for binary-search lookup; the catalog digest covers names,
descriptions, serialized schemas, effect classes, and the exposure mode.

Exposure is a compile-time decision. A catalog with at most 24 external tools
and 32 KiB of external schema is sent whole on every request (`Full`). A larger
catalog with an admitted selector is `Progressive`: requests carry the static tools plus one
`select_tools` meta-tool, and the system prompt carries a compact index of
external names, descriptions, and host readiness. The model pins tools by
keyword (`select_tools` ranks by deterministic token overlap, at most 8 matches
per call, 32 pins per run); pinned schemas join every later request in that
run, and a recovered run re-pins from the `select_tools` results already in its
transcript, so the request the provider sees after a restart matches the one
before it. Calling an unpinned external tool is a typed tool error that names
`select_tools`, never a silent lookup miss.
If explicit exposure omits `select_tools`, all permitted external schemas
are sent directly under the same catalog bounds, so every admitted tool
remains callable.

`ExternalToolHost` is the single seam for anything that is not a built-in:
`catalog_blocking` returns a generation-stamped `HostCatalog` with readiness;
`catalog_is_current` is the cheap plan-cache check; `call` runs under the
runtime's deadline and cancellation and settles with a `HostCallError`
(`Timeout`, `Cancelled`, `Unavailable`, `Overloaded`, `InvalidResult`,
`Refused`, `UnknownTool`, `ShutDown`) that the loop turns into a bounded tool
error; `shutdown` is explicit and terminal. Two hosts implement it: the root's
wired MCP registry (`qq-mcp` owns generations, typed failures, hints, and
shutdown) and `EmbeddedToolHost`, an in-process host with a frozen tool
registry, a concurrency permit, a per-call deadline, and argument (64 KiB) and
result (1 MiB) bounds. Both pass the shared conformance suite in
`qq-core::hosts::conformance`; the MCP adapter runs the availability subset
over a real stdio transport. Hosts perform no implicit retry, and host hints
never grant authority: approval policy classifies every call from the effect
class the catalog recorded for it (read-only, mutating, shell, or external),
carried on the call from admission to the gate. Every external tool is gated
like a mutation whatever its name or hints; a name the catalog does not hold is
a tool error before any policy runs.

Skills are compiled into a `SkillIndex` beside the catalog. Native `.qq/`
roots and pack roots are *disclosed*: their names and front-matter
descriptions appear in the system prompt and `load_skill` reads the body on
demand. `.agents/` and `.claude/` roots are indexed for explicit `/name`
invocation only. The index is bounded (64 entries) and its roots are
fingerprinted into the plan so a new skill file recompiles on the next load,
not on the next run.

### Agent Packs

An agent pack is a directory with a `pack.ron` manifest (`PACK_SCHEMA_VERSION
= 1`) declaring an identifier, version, optional persona file, skill and
command roots, a tool allow/deny policy, per-profile MCP subsets, and the
minimum protocol version it requires. `qq-config` discovers packs from
`<global>/packs/<id>/` and, when the project is trusted, `.qq/packs/<id>/`
root-to-leaf, plus explicit `packs:` entries; at most 32 are admitted, later
layers win by identifier, and every manifest error is a typed configuration
failure that names the pack. Pack profiles merge beneath the configuration's
own `profiles` in the same flat namespace and a name declared by both is a
conflict, not a silent override.

The root translates a selected pack into a `PackSelection` and the plan
compiles it: the persona (bounded to `MAX_PERSONA_BYTES`) prepends the system
prompt, pack skill roots join the disclosed index as `pack:<id>/...`, the tool
policy filters the catalog before exposure is decided, and the MCP subset
restricts which servers the wired host contributes. The descriptor's `pack`
section and `AgentProfileSummary.pack` name the identifier and version; a pack
that requires a newer protocol fails plan compilation with a configuration
error rather than degrading.

### Context Sources

A `ContextSource` supplies pre-turn context the runtime does not own (memory,
retrieval, project state). Sources are attached to the profile, bounded to
eight per plan, and fetched after guidance and before the first provider
request under a clamped `ContextBudget` (at most 64 KiB, 64 items, 10 s) with
a bounded LRU `ContextCache` keyed by source and query. Each fetch settles with
a `ContextSourceOutcome` (`Fetched`, `FetchedTruncated`, `Cached`,
`CachedTruncated`, `TimedOut`, `Unavailable`, `Refused`, `Invalid`) recorded on
`RunPromptIdentity.context_sources` with the content hash, so the descriptor
and the run row agree on exactly what the model saw. Blocks are appended to
the system prompt only: a source can never insert or alter transcript
messages. A failure follows the source's declared `FailPolicy`: `Open` drops
the block and records the outcome; `Closed` fails the run with
`RunFailureKind::ContextSource` before any provider work.

### Observers

Every event a client can observe is published after its durable commit. The
store encodes each envelope exactly once, inside the transaction that persists
it, and keeps that encoding as a `PublishedEvent { envelope, json }`. After the
transaction commits, the store worker appends the batch to a bounded,
sequence-indexed per-workspace ring (1024 events) that exists only while the
workspace has a subscriber; a failed transaction publishes nothing.
`SessionRuntime::subscribe_published` reads by cursor. A cursor the ring covers
attaches and catches up from memory with no store job, so reconnecting
observers and fan-out attachments do not queue behind the store worker; any
other cursor is validated and paged from SQLite in pages of
`MAX_REPLAY_EVENTS` inside one control job that also joins the ring, so no
commit can land between the page and the first live read. Live delivery is
then a ring lookup per event; a subscriber that lags past the ring capacity is
redirected to SQLite catch-up from its last cursor, so every subscriber
observes a contiguous, complete sequence at its own pace and slows only
itself. The HTTP server writes `json` into the SSE frame as-is, so a live
delivery and a replay are byte-identical and no event is serialized more than
once. `qq_client::observer::run` is the
ingestion contract for products that consume events (memory, analytics,
notifications): it owns its cursor, delivers each committed event in sequence
order to one `EventSink`, reconnects with bounded backoff (50 ms to 5 s) from
the last acknowledged cursor after transport loss, and exits with a typed
`ObserverExit` (`Stopped`, `CursorRejected` for a cursor from another store,
`EventTooLarge`) that requires a fresh snapshot rather than a silent resume.
The capability document's `events` section states the contract: post-commit
delivery, the replay page, the subscription cap, the event bound, and that
retention is unbounded.

### Structured Input And Steering

A prompt is a bounded list of `InputPart`s (text, workspace file by
reference). The transport and the durable admission path validate the parts
syntactically and perform no I/O; the transcript row carries the text with
`@path` placeholders. File parts resolve when the run starts, through the
plan's workspace capability, bounded and optionally hash-checked, and record
into the session file state. Any failure there settles the run with a typed
`invalid_command` outcome before the first provider request; the command that
queued it already succeeded, which is the point: admission stays fast and
pure, and a stale attachment is a run outcome, not a transport error.

Steering adds user input to an executing run. The session layer records the
message durably (`steering: true`, state `queued`), then hands it to the run
loop over a bounded per-run channel (`MAX_PENDING_STEERING`, the same bound
admission enforces) held in `SessionRuntimeInner.steering`. The loop applies
steering only at a boundary: after a turn's tool results are appended, or in
place of completing when the model returned no tool calls. A provider request
already sent is never rewritten. An interrupting steer bumps a per-run watch
generation the loop observes inside the provider stream `select!`, the
approval wait, and the tool execution `select!`: the in-flight future is
dropped (which kills a shell process group and abandons MCP and child
awaits), streamed text stands as the partial turn, tool calls the model had
begun are discarded because their arguments may be incomplete, and calls
awaiting approval or executing settle as interrupted with an error result so
the transcript stays provider-valid. Applied steering is persisted with the
ordinal of the turn whose request first carried it; context assembly replays
it before that turn, after the preceding tool results, so the durable
transcript and the requests the provider saw agree. Steering still queued
when a run settles is superseded in the settlement transaction. A replayed
`steer_run` returns its receipt without re-queuing.

Output truncation is a turn boundary, not a failure. Every provider adapter
maps its "stopped at the output token limit" stop reason (and Anthropic's
`pause_turn`) to the neutral `ProviderEvent::Incomplete { usage, reason }`
rather than an error; content-filter and refusal stops remain
`ProviderError::ResponseIncomplete`. The loop treats `Incomplete` like an
interrupt: streamed text stands, begun tool calls are discarded because their
arguments are incomplete, and the partial turn is committed through the same
`AssistantTurnCompleted` transaction with `truncated: true` on the turn row and
its message, so it is charged and durable before anything continues. Up to
`MAX_OUTPUT_CONTINUATIONS` (3) times per run the loop then publishes
`run_output_truncated` (the counter rides the same transaction on
`runs.output_continuations`), appends the fixed continuation notice as a user
message to keep role alternation, and issues the next turn with tools
available. Context assembly replays that notice after every truncated turn so
the durable transcript matches the requests the provider saw. Past the cap the
run settles as `provider_output_truncated`, naming the limit and turn count; a
reserved budget final response that truncates settles as the budget exhaustion
it already was and is never continued. Restart never resumes an in-flight
continuation: the committed partial turns are what the next prompt sees.

Run limits are core-owned. `BudgetMeter` charges turns, tool calls, tokens
(total, input, output), tool-output bytes, and cost as the loop observes them
and settles every accepted bound with exactly one `BudgetLimitKind`; lost usage
under any token bound is the explicit `tokens_unknown`, never a silent pass.
Child count and concurrency bounds lower the runtime ceilings for one run and
are enforced as typed spawn refusals the model can act on, not terminal
outcomes; depth is fixed at one and advertised.

Protocol codecs, request-time authorization, framing, retry policy, and
transport are internal implementation details. Shared protocol behavior is
composed from private functions and small structs; do not introduce a
`ProtocolCodec` super-trait or Template-Method hierarchy that exposes vendor
differences as hooks. Add a public seam only when two real consumers require it.
A new deployment over an existing protocol should normally require
configuration only; a new protocol should add one codec and its contract
fixtures without changing `qq-core`.

Operational probes call `ProviderCompiler::compile_for_canary`. It uses the same
recipe and adapter-selection path while disabling pre-stream HTTP/Mantle retries
so a single probe cannot spend multiple inference attempts. The checked-in
runner is `cargo xtask providers check live`; it is explicitly credentialed,
bounded, and outside normal runtime control flow.

Run `cargo bench -p qq-provider --bench provider_compiler` to measure compiled
recipe construction independently from provider network latency. End-to-end
startup and time-to-first-token benchmarks remain the primary performance
signals.

One durable run follows a guarded loop:

1. Accept, validate, and persist the queued user command.
2. Reserve one queued run without changing its public queued projection or
   publishing `RunStarted`. This unpublished coordination transaction may use
   WAL `synchronous=NORMAL`: an OS or power loss may discard only the
   reservation pointer, while the already-FULL-committed queued run remains
   eligible. The store restores `synchronous=FULL` before returning.
3. Prepare the runtime and conservatively plan the complete provider request
   under the acquired permit; cancellation remains durable and observable.
4. In one guarded transaction, persist the resolved model, prompt identity,
   exact request measurement, running/session/message state, and `RunStarted`.
5. Re-read cancellation, then poll the provider only after that transaction
   commits and publishes.
6. Execute requested tools under the session's workspace policy, persisting
   resulting messages and events before publishing them.
7. Repeat until completion, cancellation, compaction, or failure.

This ordering makes persisted state authoritative and allows clients to resume
an event stream without losing output. Each completed model turn also commits
the run's cumulative usage and estimated cost.

The store worker serves two bounded lanes. Control jobs (commands, claims,
snapshots, catch-up reads) each run in their own transaction and reply as soon
as it commits, so an acknowledgement never waits behind streamed output.
Output jobs (text, reasoning, tool output, turn commits) are group-committed:
when the worker dequeues one it opens a single transaction, runs that job and
every output job already queued behind it (at most `OUTPUT_GROUP_LIMIT = 16`,
and stopping early when a control job is waiting) each inside a savepoint,
commits once, and only then publishes their events and replies to every
caller. A failing job rolls back its own savepoint and its siblings are
unaffected; a failing outer commit fails every job in the group with
`Persistence` and publishes nothing. Eight concurrent streams therefore cost
one fsync per service round instead of eight, at `synchronous=FULL`
throughout. Operation code is identical in both modes: every mutation begins a
`Unit` that is a transaction when alone and a savepoint inside a group.

Command acknowledgement is bounded work independent of history. The active
run's activity is a `runs.activity` column written in the same transaction as
its `RunActivityChanged` event, so the session summary a command publishes
reads one row instead of scanning the event log; the command bound is a
maintained `metadata.command_count` counter rather than a `COUNT(*)` per
command; and a workspace snapshot aggregates every session's accounting in one
grouped query. A claimed run carries the cancellation flag, session file
hashes, and pending steering out of the claim transaction, so claim to first
provider request is two store hops (claim, then `RunStarted`), and context
assembly runs a fixed number of session-scoped queries rather than one per
message and per turn. Workspace path canonicalization runs on a blocking
thread before the command reaches the store worker.

Caller budgets are core-owned. `submit_prompt.limits` carries a versioned
`RunLimits` (wall clock, model turns, tool calls, total tokens, cost) that is
validated at admission, persisted with the run row, and metered by the runtime
loop: every provider turn is decided at the turn boundary and the wall clock
also bounds a provider stream that never yields. A cost cap without configured
pricing is rejected before provider work. When the countable budget is nearly
spent the last permitted turn becomes a tool-free final status response; an
elapsed wall clock or a provider turn that omits usage under a cost cap settles
immediately. Every bound produces the typed `budget_exhausted` outcome, never a
provider failure, so the TUI, server, and headless adapter observe one
contract. Each sequential child admission, including an auditor, derives fresh
remaining cost and token bounds after charging earlier children. A turn containing
children executes sequentially when the parent has any finite cost or token
bound; unbounded and duration-only read fanout keep their existing concurrency.
These are observed-spend limits: a provider turn or reserved final response can
still overshoot; there is no prepaid reservation or estimated audit minimum.

Children share the parent's absolute deadline. Preflight and queueing consume
that clock; expiry cancels the owned work and waits for cleanup, which can finish
after the deadline. Creation accepted by the store may commit after expiry and
is then cancelled under the same retained owner. Settled child receipts charge
that run and its exact owned descendants, excluding later user prompts in child
sessions. Unknown usage or cost stays unknown and refuses a new child when the
corresponding bound is imposed. Descendant history cannot be deleted until every
owning ancestor run settles. Auditors store their direct spend on their own run;
the parent meter and inclusive accounting each include that spend once.
A read-only session is not offered the mutating, shell, or non-read external
schemas its policy would deny, and a spawned child's approval mode can be
lowered but never raised by a client command.

A run may cross multiple bounded internal execution slices. The strict
256-tool-call ceiling is a runaway-loop backstop for one slice, not a
task-completion signal. Before a bounded provider turn could push a slice past
that ceiling, the runtime requests a tool-free checkpoint, requires and
persists that assistant turn, resets the slice counter, and continues the same
run with tools restored. Clients observe no terminal run event at the slice
seam. Genuine completion, explicit caller budgets, cancellation, and failures
remain the only user-level terminal conditions; provider adapters do not
participate in slice rollover.

Once prompt submission commits, the runtime owns that accepted run until it
persists exactly one terminal `RunFinished` event. Before settling started
execution, it drops dispatch and drains owned child tasks and local tool work.
A run-task panic becomes a durable server failure only after that cleanup
succeeds; unconfirmed cleanup makes the runtime unavailable instead. A headless output or trace failure requests ordinary
durable cancellation and waits for the matching terminal event; dropping the
headless owner starts the same bounded cleanup in the background. Explicit
runtime shutdown first closes command and child-run admission and stops run
claiming, then cancels every queued or running run and waits for the store to
report no unfinished work and for admitted preparation and execution to exit.
Snapshots and subscriptions remain readable while
and after shutdown so callers can observe the settled state. An embedded HTTP
owner first stops accepting connections, settles the runtime, and only then
waits for a bounded response drain; a long-lived SSE subscriber cannot prevent
accepted runs from reaching their durable terminal state.

Process loss is the final backstop rather than a reason to replay uncertain
work. Opening the store transactionally marks abandoned running runs and their
in-flight tool calls interrupted before scheduling resumes. Committed turns
remain authoritative, interrupted side effects are never re-executed, and
queued work that never started may still be claimed normally.

`spawn_agent` creates an authority-limited child session, its queued prompt run, and the
parent-run ownership link in one store transaction. The ordered
`SessionCreated` and `PromptQueued` events are published only after that fully
initialized state commits. The child is therefore either absent or durably
owned and claimable; it cannot survive a failed submission as an idle orphan.
If the process stops before a queued child is claimed, recovery cancels that
child when it interrupts the owning parent. Parent cancellation uses the same
durable ownership link for in-process children. Once a child completes, only
its final committed model turn's text or refusal is returned to the parent;
earlier turns remain visible in the child's authoritative transcript.

An owned child task retains admission, loader work, and the writer permit even
if an interrupting parent drops its result waiter. Accepted creation is awaited
through its transaction reply. Child outcome reads and cancellation wait for
bounded control-lane capacity; a hard failure preserves uncertain ownership and
fails the runtime. Child spend remains available until the parent charges and
acknowledges it, including when steering interrupts a completed result. Audits
use the same child ownership and accounting boundary.

Local tool ownership extends through blocking file work and shell termination
and reap. A parent, or a replacement run in its session, cannot start another
write while a supervised child is draining. Cancellation does not undo a native
write already inside its atomic apply step. Unix shell process groups are killed
on interruption; Windows explicitly terminates the owned child process. Detached
or escaped processes and remote MCP effects are not proven undone by stopping
QQ dispatch, and uncertain effects are never implicitly retried.

Which model a child runs is decided at one choke point from the delegation
roster. The root translates the configured `delegation` section (or, as sugar,
a legacy `worker_model`) into the secret-free `qq_protocol::DelegationRoster`
on the `AgentProfile`: at most eight routes, each with an operator-declared
role (`fast`, `balanced`, `strong`), an optional note, catalog context window
and output limit, and its blended price relative to the spawning model. The
plan records the roster in its descriptor (version 4), renders it once into the
Delegation section of the system prompt together with the spawning model's own
route and context window, and compiles `spawn_agent` with a `role` argument
plus an exact `model` override limited to roster routes (the declaration is
bounded at 2 KiB). Depth follows the roster too: a run receives a spawner while
its session `depth` is below the roster's `max_depth` (1 by default, 3 the
runtime ceiling), so children may delegate when configured and the deepest
level is refused at dispatch. Only depth-one children may hold write
authority; grandchildren are read-only by construction. Every session records
its `depth` and `root_run_id`; one root's tree is capped at
`MAX_DESCENDANTS_PER_ROOT` (24) sessions, cancellation and recovery cascade
over the whole subtree by a bounded recursive query, inclusive accounting sums
the same subtree, and each depth claims runs from its own permit pool so
parents awaiting children at any level cannot starve the level below.

A root run's candidate final answer may be audited before it completes. The
plan carries an `AuditPolicy` (`off`, `heuristic`, or `always`; `heuristic` is
the configured default and fires on a mutation, a non-read shell command,
twelve tool calls, or a spawned child). At the completion boundary the loop
consults an `AuditHook`; the session layer implements it by spawning a
read-only child with `purpose: audit` at the roster's audit role, whose brief
is the user prompt, the answer, and a bounded action list, never the
transcript. The child verifies the claims against the workspace with its own
tools and answers one JSON verdict. `pass` completes the run; `revise` pushes
the answer plus the findings as a runtime notice and continues once (bounded by
`max_revisions`); an auditor that fails, is refused, or answers prose is
`unavailable` and the answer stands, provided the parent's budget still permits
completion after charging audit spend. Otherwise the run settles as
`budget_exhausted`, even when the auditor passed. The record is durable on the run
(`runs.audit_json`, published as `run_audit_completed`) before the run settles,
and the child's spend is charged to the audited run. Children, internal runs,
budget-final turns, and runs without a positive, known remainder for each imposed
cost/token bound or with an expired deadline are never audited. Unbounded
families do not require an affordability estimate. At dispatch
`resolve_delegation_route` applies explicit
model, then role, then the roster's default role; without a roster the legacy
worker/parent fallback and full authenticated route list remain, and the
session spawner still validates every resolved route against the authenticated
served model list before any durable child state exists. Roles are declared,
never inferred: QQ ranks nothing.

Compaction is a property of that projection, not an edit to the transcript: a
validated summary row and cutoff marker commit atomically with the internal
summarization run, three compactions are retained per session for
`RollbackCompaction`, and a summary that is empty, missing a required section,
or fails to shrink the measured assembly settles as a policy failure while the
prior compaction stays in force. `search_history` is the recall path that makes
aggressive compaction safe: session runs may search the complete durable
transcript, including spans compaction replaced, for bounded cited excerpts.
Direct runs have no durable transcript and do not see the tool.

Future model requests, capacity accounting, and compaction all consume the same
provider-neutral projection of that durable state. The projection retains
committed prompts and model turns, pairs every replayed tool call with exactly
one persisted or synthesized result, and appends a clearly labelled QQ runtime
notice after failed, cancelled, or interrupted runs. These notices are model
context only: they do not become transcript messages or pretend to be user
instructions. Calls known to have started receive an interrupted result; calls
that never started receive a deterministic not-executed result, so recovery
preserves progress without retrying uncertain side effects.

## HTTP And SSE Protocol

Clients issue versioned HTTP requests with JSON bodies. The server streams
ordered events using Server-Sent Events (SSE). HTTP keep-alive and one
long-lived SSE connection per attached client avoid repeated connection setup.

The initial protocol needs operations equivalent to:

```text
POST /v1/sessions
GET  /v1/sessions/{session_id}
POST /v1/sessions/{session_id}/messages
POST /v1/runs/{run_id}/cancel
POST /v1/approvals/{approval_id}
GET  /v1/sessions/{session_id}/events
```

The final resource names belong in a protocol specification. These routes only
establish the required behaviors.

Every streamed event has:

- A monotonically increasing event ID within its stream.
- A session ID and, when applicable, a run ID.
- A stable event type.
- A versioned JSON payload.

Clients reconnect with `Last-Event-ID`; the server replays persisted events
after that ID before switching to live delivery. Heartbeats keep idle streams
detectable. Mutating requests carry request IDs or idempotency keys so retries
cannot accidentally duplicate work.

SSE is intentionally server-to-client. Client commands, approvals, and input
remain normal HTTP requests. Do not add GraphQL, raw TCP, gRPC, WebRTC, or
WebSocket initially. WebRTC is especially unnecessary because Tailscale
already provides private connectivity and NAT traversal. WebSocket may be
considered later only if an implemented feature, such as a full interactive
PTY, cannot be expressed cleanly through HTTP and SSE.

JSON is the initial wire format. Binary serialization should replace it only
after profiling demonstrates that serialization or bandwidth is material.

## Persistence

SQLite is the initial and default store. It provides fast local durability,
transactions, simple deployment, and no external service. Use WAL mode and
keep blocking database work off Tokio executor threads, preferably behind a
small storage module using a dedicated thread or bounded blocking work.

The store must preserve at least:

- Sessions and their workspace identity.
- User, assistant, and tool messages.
- Runs and terminal outcomes.
- Ordered events required for SSE replay.
- Model/provider metadata needed to explain and resume a session.

Schema migrations are part of the binary. Chat history must survive process
restarts, and a failed write must not be presented to clients as durable. Do
not introduce an external database until measurements show SQLite is the
bottleneck.

## Workspaces And Tools

The server executes tools on the machine where it runs. In local `qq` mode,
the workspace defaults to the canonical current working directory. Tool paths
must remain within the selected workspace unless the user explicitly grants
wider access.

The first useful tool set is deliberately small:

- Read files and directories.
- Search file names and contents.
- Apply explicit file changes.
- Execute bounded shell commands.

Tool calls and results are persisted and streamed so the user can understand
what the agent did. Destructive or externally visible operations require an
approval policy; the exact policy is defined in `tools.md`.

A remote server can initially operate only on workspaces available on that
server. A hosted coordinator plus outbound-connected desktop workers is a
possible later architecture, but it is not part of the initial implementation.

## Concurrency And Multiple Agents

Parallel model requests are mechanically simple; useful parallel agents are
not. The server must eventually account for rate limits, token budgets,
cancellation, duplicate work, context exchange, and conflicting changes.

Initial concurrency should therefore be bounded and session-aware. Multiple
independent sessions may run concurrently, but two writing agents must not
modify the same checkout concurrently. When *parallel* editing subagents are
introduced, each receives an isolated Git worktree or sandbox and returns a
patch for central review and integration. A single serialized `Supervised`
write child shares its parent's checkout: the parent is blocked while it runs,
sibling writers serialize on a per-run permit retained through local execution
teardown, and every mutating call it makes
is adjudicated before it executes. Read-only research agents may be
parallelized earlier.

Do not build an agent swarm, distributed scheduler, or worktree coordinator in
the initial version.

## Local And Remote Networking

The server binds to loopback by default. Binding to a Tailscale address or
another non-loopback interface must be explicit. Tailscale supplies encrypted
private networking and device-level access controls, but remote command
execution still needs an application authentication and authorization decision
before it is enabled broadly.

The same HTTP/SSE protocol serves local TUI clients, remote TUI clients, and
future browser or mobile clients. Protocol replay means moving between devices
does not require transferring in-memory client state.

## Hosting Boundary

QQ is designed to be run by a **supervisor**: a batch runner, CI job,
evaluation harness, or hosted service that launches `qq run` inside an
environment it controls and consumes the JSONL record stream and exit code.
[`headless-contract.md`](./headless-contract.md) fixes that contract and the
division of responsibility.

The division is: QQ owns everything that must work on one machine for one
user with no network other than the model endpoint — the agent loop,
providers, tools, approvals, run limits, the durable session store, events,
and the typed outcome. The supervisor owns everything that needs more than one
tenant, more than one worker, or an authoritative record of money — isolation,
repository checkout, patch extraction, spend authority, attempts and leases,
independent verification, artifacts, identity, and billing.

Three rules follow:

- A supervisor consumes QQ as a binary through argv, environment, inline
  configuration, and stdout. It never links `qq-core` into a process that
  also executes untrusted repository code.
- QQ has no supervisor-only mode and no product vocabulary. A capability a
  supervisor needs is added only in a form a local user, a CI job, and an
  evaluation harness could also use. CLI plumbing and compilation stay off
  the run hot path; generic opt-in completion validation and bounded repair
  belong in core, preserve default behavior when disabled, and require
  cancellation, budget, and performance acceptance.
- The headless contract is public and pinned by fixtures in this repository so
  a supervisor can test against it without reading QQ source.

## Performance Discipline

Optimize end-to-end time to a useful result, not isolated microbenchmarks.
Measure at least startup time, command acknowledgement, time to first model
token, tool execution, persistence latency, reconnect/replay time, memory, and
render responsiveness.

Keep hot paths direct, queues bounded, and interfaces small. Avoid speculative
abstractions and serialization layers. Any complexity introduced for speed
must be supported by a benchmark and must not make routine development hostile.

## Intentionally Deferred

The initial repository is pure Rust. Do not create or scaffold any of the
following yet:

- React or other web frontend.
- Native or cross-platform mobile application.
- JavaScript/TypeScript packages or package workspace.
- Separate server executable.
- Distributed workers or cloud control plane. These belong to a supervisor
  above the headless contract; see "Hosting Boundary".
- Plugin marketplace or public extension interface.
- Multi-user tenancy. Same boundary.
- Multi-agent editing orchestration.

The HTTP/SSE server and client crates are designed to permit future surfaces,
but future client code must not add placeholder crates or speculative
extension points before it exists.
