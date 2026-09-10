# QQ Protocol Specification

## Purpose

This document specifies the versioned HTTP/SSE wire protocol between QQ
clients and a QQ server. It is the contract for the TUI, future remote
clients, and any automation that talks to `qq serve`.

The protocol is transport-neutral in the `qq-protocol` crate: shared types do
not depend on an HTTP client or server framework. `qq-server` maps those types
onto HTTP routes and SSE frames, while `qq-client` performs the inverse mapping.

Canonical source of truth for schemas and tags:

- `crates/qq-protocol/src/lib.rs`
- `crates/qq-protocol/src/sessions.rs`
- `crates/qq-protocol/src/ids.rs`
- shared request and event limits in `crates/qq-protocol/src/limits.rs`
- local connection capability in `crates/qq-protocol/src/local.rs`
- route wiring in `crates/qq-server/src/lib.rs`
- client decoding in `crates/qq-client/src/lib.rs`

Related documents:

- `docs/design/architecture.md` — system shape and transport choices
- `docs/design/tools.md` — tool loop, approvals, and security policy
- `docs/design/transcript.md` — client presentation of session history

## Design Principles

1. **Commands in, events out.** Clients mutate state with ordinary HTTP POST
   requests. The server pushes ordered history over SSE. There is no
   bidirectional socket protocol.
2. **Persisted state is authoritative.** The server commits events before
   publishing them. A client may disconnect and resume without losing work.
3. **Idempotent mutation.** Every command carries a client-generated
   `command_id`. Retries with the same id and payload return the original
   receipt; a conflicting payload is rejected.
4. **Workspace-scoped streams.** Event sequences belong to a workspace, not a
   single session, so one subscription can observe every session in that
   workspace.
5. **Stable, versioned JSON.** Enums use externally tagged `type` fields with
   `snake_case` names. Unknown fields are rejected on request bodies that use
   `deny_unknown_fields`.

## Protocol Version

```text
PROTOCOL_VERSION = 17
```

The counter restarted at 1 on 2026-07-28, before any release; earlier
values (1–12) belonged to pre-release iterations and no released build
speaks them. The number is a build-compatibility counter, not a product
version — being "high" carries no meaning. Version 2 added session
compaction (`compact_session`, `session_compacted`); version 3 added
run context occupancy (`run_context_updated`,
`RunSnapshot.context_tokens`); version 4 added workspace-lifetime
approvals (the `approve_for_workspace` decision, the
`approved_for_workspace` resolution, and the
`workspace_grant_promoted` event); version 5 made context occupancy
authoritative session state (`SessionSummary.context_tokens` and
`session_context_updated`) so legacy billing totals and internal compaction
runs cannot masquerade as the current session context, and added explicit
direct and inclusive session accounting totals; version 6 added persisted
run prompt identity (`RunSnapshot.prompt_identity`); version 7 added the
persisted `model_turn_completed` event so each provider turn records its model,
usage, and estimated cost before it is published; version 8 added the optional
immutable resolved-model descriptor to `RunSnapshot`; version 9 added its
optional provider request-shape identity. Version 9 is required because older
snapshot decoders reject the nested field under `deny_unknown_fields`, even
though historical version-1 descriptors and deployments that cannot produce a
secret-free identity omit it. Version 10 added core-owned run budgets: the
optional `submit_prompt.limits` request field, `RunSnapshot.limits`, the
`budget_exhausted` run status, and the typed `budget_exhausted` run outcome.
Older clients reject both the new outcome tag and the new snapshot field.
Version 11 added compaction rollback: the `rollback_compaction` command, the
`compaction_rolled_back` outcome, and the `session_compaction_rolled_back`
event. Older clients reject the new command and event tags. Version 12 added
optional multi-session client support: `SessionSummary.spawned_by` (the parent
run and `spawn_agent` call that created a child), `SessionSummary.activity`
(the active run's latest liveness state), and `SnapshotRequest.include_sessions`
with the matching `WorkspaceSnapshot.included` bodies. Every addition defaults
when absent, but older `deny_unknown_fields` decoders reject summaries that
carry the new fields. Version 13 completed the backend contract: `submit_prompt`
carries a bounded `input` part list instead of a `prompt` string; sessions
select a configured `profile` (`create_session.profile`,
`set_session_profile`, `SessionSummary.profile`); accepted runs record their
plan identity (`run_started.plan`, `RunSnapshot.plan`); active runs take
`steer_run` input with optional interruption and the matching
`steering_queued`/`steering_applied`/`steering_superseded`/`run_interrupted`
events; `RunLimits` gained input/output token, tool-output-byte, and child
bounds with their `BudgetLimitKind`s; sessions and runs carry opaque
`correlation`; `POST /v1/capabilities` describes the server; and `ServerInfo`
plus the capability document tolerate unknown fields so version skew is
reported rather than failing to decode. Version 14 froze the tool catalog into
the plan: `RunPromptIdentity` gained `catalog_digest`, `exposure`
(`full`/`progressive`), and `context_sources` (one `ContextSourceRecord` per
attached source with its typed outcome and content hash); `RunFailureKind`
gained `context_source`; `RunPlanIdentity.descriptor_version` is 3 because the
descriptor now carries the catalog, skill index, and pack sections; the
capability document gained `tools`, `events`, and the per-workspace
`workspace_tools` section; and `AgentProfileSummary` gained `pack`. Version 15
added `SessionSummary.approval_mode` so every client renders the policy a
session holds tool calls against, and `set_approval_mode` now publishes
`session_updated` with the new summary. The field defaults to `auto` on decode
so events persisted by earlier builds still replay; a version-14 client rejects
the field on every summary-bearing snapshot and event. Version 16 added bounded
output continuation: when a provider stops a turn at its output token limit the
runtime commits the partial turn, publishes `run_output_truncated` (turn ordinal
and 1-based continuation count), and resumes on the next turn, up to
`LimitCapabilities.max_output_continuations` times before settling with the
new `provider_output_truncated` failure kind. `MessageSnapshot.truncated` marks
the assistant message the provider cut. Version 16 also carries the delegation
and audit work: `SessionSummary.approval_mode` gained `supervised`,
`SessionSummary.purpose` (`task`|`audit`) and `SpawnOrigin.depth` were added,
`RunSnapshot.audit` records how a run's final answer was audited, and the
`run_audit_started` and `run_audit_completed` events were added (see
[Final-answer audit](#final-answer-audit)). Older clients reject the new event
tags and failure kind. Version 17 added the server's durable identity to
`ServerInfo`: `server_id` (the store id every cursor from that server carries)
and a bounded printable `display_name`. A client keys a saved server profile
by `server_id`, never by address, and can confirm that a cursor belongs to the
server it is talking to before replaying it. Older clients tolerate the new
fields on decode but their discovery metadata format is rejected. Golden
fixtures live under `crates/qq-protocol/tests/fixtures/v17/`.

Clients and servers must agree on this value.

- `GET /v1/health` returns `ServerInfo.protocol_version`.
- Local discovery metadata also records the protocol version. A mismatch is a
  hard failure; clients must not speak to an incompatible server.
- Bump `PROTOCOL_VERSION` whenever an externally visible wire change is not
  backward compatible for existing clients (new required fields, removed
  variants, changed tag names, changed cursor format, and similar).

## Transport

| Concern | Choice |
| --- | --- |
| Scheme | HTTP/1.1 over loopback by default (`http://127.0.0.1:<port>`) |
| Commands | `POST` with `Content-Type: application/json` |
| Snapshots / catalog | `POST` with JSON request and JSON response |
| Live history | `GET` Server-Sent Events (`text/event-stream`) |
| Auth | `Authorization: Bearer <token>` on every request |
| Body limit | 1 MiB request bodies |
| Keep-alive | SSE comment/`keep-alive` every 15 seconds while idle |

Intentionally not used: GraphQL, gRPC, raw TCP, WebRTC, or WebSocket. JSON is
the only wire encoding until profiling shows serialization or bandwidth is a
real bottleneck.

### Authentication

The server generates a random bearer token at startup and writes it, with the
bind address, process metadata, and server identity, to a private per-user
file (`server.ron`, format version 2). The file is published only once the
runtime has opened and the identity is known; an unstarted reservation holds
the instance lock and listener but advertises nothing.
The binary discovers the running instance through `qq-server` and passes a
redacted `LocalServerConnection` capability to `qq-client`, which attaches with:

```http
Authorization: Bearer <token>
```

Missing or incorrect credentials receive `401` with:

```json
{ "error": "authentication required" }
```

Token comparison is constant-time. Metadata and tokens must never be logged in
full by clients or servers.

### Error Responses

Failed HTTP requests return JSON:

```json
{ "error": "<stable message>" }
```

Common status codes:

| Status | Meaning |
| --- | --- |
| `400` | Malformed body, wrong command for the route, invalid cursor/id |
| `401` | Missing or invalid bearer token |
| `404` | Unknown path |
| `405` | Wrong HTTP method |
| `413` | Body exceeds the 1 MiB limit |
| `503` | Handler unavailable or concurrency limit reached |
| `500` | Unexpected internal failure |

Error bodies intentionally avoid leaking internal details.

### Concurrency Limits

The HTTP adapter admits at most:

- 64 concurrent session/command/snapshot/catalog requests
- 64 concurrent workspace event subscriptions

Additional requests receive `503`.

## Resource Model

```text
Store
 └── Workspace (canonical filesystem path)
      └── Session
           └── Run
                ├── Message (user / assistant)
                └── ToolCall
```

| Resource | Identity | Notes |
| --- | --- | --- |
| Store | `store_id` | One durable SQLite store per server process data dir |
| Workspace | `workspace_id` | Canonical absolute path known to the server |
| Session | `session_id` | Conversation within a workspace; optional `parent_id` |
| Run | `run_id` | One queued/executed model attempt for a prompt |
| Message | `message_id` | User or assistant text for a run |
| Tool call | `tool_call_id` | One model-requested tool invocation |
| Command | `command_id` | Client idempotency key for one mutating request |

### Identifiers

All resource ids are 16 random bytes encoded as **32 lowercase hex
characters** with no separators:

```text
00112233445566778899aabbccddeeff
```

Uppercase hex, odd lengths, and non-hex characters are rejected. Ids are
generated by the party that creates the resource:

- Clients generate `command_id` values.
- The server generates workspace, session, run, message, and tool-call ids.

### Event Cursor

Workspace event order is a single monotonic sequence:

```text
EventCursor = { store_id, workspace_id, sequence }
wire form   = "<store_id>:<workspace_id>:<sequence>"
```

Example:

```text
aa..aa:bb..bb:42
```

Rules:

- `sequence` increases by one for each committed event in that workspace.
- Cursors are opaque to clients except for comparison and reconnect.
- A cursor from a different store or workspace must not be reused on another
  subscription.

## HTTP Routes

All routes require authentication. Command routes accept a `CommandRequest`
and return a `CommandReceipt` unless noted.

```text
GET  /v1/health
POST /v1/capabilities
POST /v1/workspaces/resolve
POST /v1/workspaces/snapshot
POST /v1/models
POST /v1/sessions
POST /v1/sessions/prompts
POST /v1/sessions/approval-mode
POST /v1/sessions/model
POST /v1/sessions/profile
POST /v1/sessions/delete
POST /v1/sessions/prune
POST /v1/sessions/compact
POST /v1/sessions/compact/rollback
POST /v1/runs/steer
POST /v1/runs/cancel
POST /v1/tools/approvals
GET  /v1/workspaces/{workspace_id}/events
```

Each command route accepts only its matching `SessionCommand` variant. Sending
the wrong variant to a route is a `400`.

### `GET /v1/health`

Liveness and version probe used by discovery.

Response `ServerInfo`:

```json
{
  "protocol_version": 17,
  "version": "0.1.0",
  "pid": 12345,
  "server_id": "0123456789abcdef0123456789abcdef",
  "display_name": "build-box"
}
```

`server_id` is the store identity (`EventCursor.store_id`); it survives
restarts, endpoint changes, and token rotation. `display_name` is at most 64
bytes, printable, trimmed, and carries no identity; the server uses a
configured label, else the host name, else `qq`.

`ServerInfo` tolerates unknown fields so a client built against an older
revision still reads a newer server's answer and reports the mismatch.

### `POST /v1/capabilities`

The versioned capability document. Clients format supported behavior from it
rather than inferring it from provider names or trial commands. The request is
optional-bodied:

```json
{ "workspace_id": "..." }
```

Response `ServerCapabilities` (abridged; see
`crates/qq-protocol/tests/fixtures/v15/capabilities.json` for the full golden):

```json
{
  "version": 1,
  "protocol_version": 16,
  "server_version": "0.1.0",
  "input_parts": ["text", "workspace_file"],
  "commands": ["resolve_workspace", "create_session", "submit_prompt", "steer_run", "..."],
  "steering": { "boundary": true, "interrupt": true, "max_pending_per_run": 4 },
  "limits": {
    "supported": ["duration", "model_turns", "tool_calls", "total_tokens", "cost", "cost_unknown",
                  "input_tokens", "output_tokens", "tokens_unknown", "tool_output_bytes"],
    "max_request_bytes": 1048576,
    "max_event_bytes": 1048576,
    "max_input_parts": 32,
    "max_input_text_bytes": 131072,
    "max_input_file_parts": 8,
    "max_input_file_bytes": 262144,
    "max_pending_prompts": 16,
    "max_children": 8,
    "max_concurrent_children": 3,
    "max_child_depth": 3,
    "max_correlation_entries": 8,
    "max_output_continuations": 3,
    "max_descendants": 24
  },
  "approvals": ["approve_once", "approve_for_session", "approve_for_workspace", "deny"],
  "approval_modes": ["read_only", "ask", "auto", "full"],
  "profiles": [
    { "id": "default", "model": "openai/gpt-5.6", "approval_mode": "auto" },
    { "id": "review", "model": "anthropic/claude-x", "approval_mode": "read_only",
      "pack": { "id": "review-kit", "version": "1.2.0" } }
  ],
  "tools": {
    "max_catalog_tools": 512, "max_tool_schema_bytes": 16384,
    "max_catalog_schema_bytes": 1048576, "full_exposure_tools": 24,
    "full_exposure_schema_bytes": 32768, "max_pinned_tools": 32,
    "max_indexed_skills": 64, "external_prefixes": ["mcp__", "ext__"]
  },
  "workspace_tools": {
    "catalog_digest": "cccc…", "exposure": "progressive",
    "hosts": [{ "name": "mcp", "generation": 3, "tool_count": 40, "ready": true }],
    "excluded_tools": 1,
    "skills": {
      "digest": "dddd…", "indexed": 2, "disclosed": 1,
      "entries": [
        { "name": "deploy", "kind": "command", "source": ".qq/commands/deploy.md",
          "description": "Ship the current branch.", "disclosed": true },
        { "name": "audit", "kind": "skill",
          "source": "pack:review-kit/skills/audit/SKILL.md", "disclosed": false }
      ]
    }
  },
  "events": {
    "post_commit": true, "replay_page": 128, "max_subscriptions": 64,
    "max_event_bytes": 1048576, "retention_bounded": false
  },
  "delegation": {
    "roster": [
      { "route": "openai/gpt-5-mini", "role": "fast", "note": "lookups, breadth",
        "context_window": 400000, "max_output_tokens": 16384,
        "relative_cost_permille": 150 },
      { "route": "openai/gpt-5.6", "role": "balanced", "context_window": 400000,
        "relative_cost_permille": 1000 }
    ],
    "default_role": "balanced", "max_depth": 1, "write_children": false,
    "max_roster_entries": 8, "max_depth_ceiling": 3
  }
}
```

`version` is the document schema version; additive fields do not bump it. The
response tolerates unknown fields (a newer server may add sections). `profiles`
is present only when the request named a workspace, because profiles come from
that workspace's layered configuration; `default` is always first and reflects
the top-level configuration. Provider and model capabilities stay on
`POST /v1/models`. Every bound is the constant the transport or runtime
enforces.

`tools` states the catalog bounds every plan is compiled under and the
external name prefixes. `workspace_tools`, like `profiles`, is present only for
a named workspace: it summarizes that workspace's default plan — the catalog
digest, whether requests carry every schema (`full`) or an index plus
`select_tools` pins (`progressive`), each external host's generation, admitted
tool count, and readiness, how many declared tools were excluded, and the skill
index. `skills.entries` (additive; absent from older servers) lists every
indexed document with its slash `name`, `kind` (`command` or `skill`),
workspace-relative or `pack:<id>/…` `source`, front-matter `description`, and
whether the model may load it itself (`disclosed`); it is bounded by
`tools.max_indexed_skills` entries with descriptions of at most 512 bytes, so a
client can offer slash completion and a listing without reading the workspace.
`events` is the observer contract: events are published only after their
durable commit, a subscriber is served `replay_page` events per page from its
cursor, at most `max_subscriptions` SSE subscriptions are accepted (503 beyond),
and `retention_bounded: false` means a cursor from any point in the store's
history replays.

`delegation` (additive; present only for a named workspace) is the roster the
workspace's configuration declares for `spawn_agent`: each entry's canonical
route, its operator-declared `role` (`fast`, `balanced`, or `strong`; roles are
never inferred), optional note, catalog context window and output limit, and
`relative_cost_permille` — the route's blended per-token price relative to the
workspace's default model in thousandths (1000 = same price; absent when
either price is unknown). `default_role` is what the model gets when it names
no role; `max_depth` and `write_children` are the configured recursion and
authority bounds; `max_roster_entries` and `max_depth_ceiling` are the
runtime's hard ceilings. An empty roster means the legacy `worker_model`/parent
fallback. The same roster is recorded in the run's plan descriptor (descriptor
version 4), so a roster change recompiles the plan.

### Command envelope

Request:

```json
{
  "command_id": "<command_id>",
  "command": { "type": "<variant>", "...": "..." }
}
```

Response:

```json
{
  "command_id": "<command_id>",
  "committed_through": {
    "store_id": "<store_id>",
    "workspace_id": "<workspace_id>",
    "sequence": 0
  },
  "outcome": { "type": "<variant>", "...": "..." }
}
```

`committed_through` is the latest workspace cursor the command durable-ized.
Clients use it to advance local ack state even when the command emits no new
session events (for example, resolving an already-known workspace).

### `POST /v1/workspaces/resolve`

```json
{
  "command_id": "...",
  "command": {
    "type": "resolve_workspace",
    "path": "/absolute/or/relative/path"
  }
}
```

Outcome:

```json
{
  "type": "workspace_resolved",
  "workspace_id": "..."
}
```

The server canonicalizes the path, requires a directory, and reuses the
existing workspace row when the path was seen before.

### `POST /v1/sessions`

```json
{
  "command_id": "...",
  "command": {
    "type": "create_session",
    "workspace_id": "...",
    "parent_id": null,
    "model": {
      "model": "provider/model-id",
      "max_output_tokens": 8192,
      "organization": null
    },
    "approval_mode": "ask",
    "profile": "review",
    "correlation": { "thread": "t-1" }
  }
}
```

| Field | Required | Notes |
| --- | --- | --- |
| `workspace_id` | yes | From `resolve_workspace` |
| `parent_id` | no | Optional parent session |
| `model` | yes | `ModelSelection`; fields inside may be omitted |
| `approval_mode` | no | Defaults to `auto` |
| `profile` | no | `AgentProfileId`; defaults to `default`. Lowercase letters, digits, hyphens; ≤ 64 bytes |
| `correlation` | no | Opaque string map, ≤ 8 entries, keys ≤ 64 B, values ≤ 256 B, ≤ 2 KiB total |

The profile names a bundle of defaults declared in the workspace
configuration's `profiles` map (model, organization, output cap, approval
mode). It is recorded on the session and applied when each run is claimed:
the session's explicit `model` selection wins over the profile, which wins
over the top-level configuration. A profile the configuration does not
declare fails the run at claim time with a `configuration` failure naming the
profile; `POST /v1/capabilities` lists the profiles a workspace declares.
`correlation` is stored and echoed on `SessionSummary` for attribution; QQ
never interprets it or treats it as authorization.

Outcome:

```json
{
  "type": "session_created",
  "session_id": "..."
}
```

Emits `session_created`. The workspace's effective config grants (see
`docs/design/tools.md`, "Grant Lifetimes") are copied into the new
session's grant set inside the creation transaction; a later config edit
affects only sessions created afterwards.

### `POST /v1/sessions/prompts`

```json
{
  "command_id": "...",
  "command": {
    "type": "submit_prompt",
    "session_id": "...",
    "input": [
      { "type": "text", "text": "Explain how sessions are stored." }
    ]
  }
}
```

`input` is a bounded list of typed parts:

| `type` | Fields | Notes |
| --- | --- | --- |
| `text` | `text` | Verbatim user text |
| `workspace_file` | `path`, optional `expected_hash` | A workspace-relative file attached by reference |

Bounds, enforced by the transport before the handler and again by the runtime
before durable admission: 1–32 parts; text totals ≤ 128 KiB and is not all
whitespace unless a file is attached; ≤ 8 file parts; paths are non-empty,
relative, NUL-free, ≤ 4 KiB. Admission performs no I/O. File parts are read
through the session's workspace capability when the run starts: each file must
be a regular UTF-8 file inside the workspace of ≤ 256 KiB, the resolved message
must total ≤ 1 MiB, and when `expected_hash` (hex SHA-256) is present the bytes
must still hash to it. Any violation fails the run before its first provider
request with an `invalid_command` failure naming the path; the command that
queued it succeeded. The transcript row (`prompt_queued.message.output`)
carries the text parts verbatim and each attachment as an `@path` placeholder;
the model sees the file contents fenced after the text. Attached files are
recorded in the session file state, so a later edit satisfies the
read-before-write rule without a redundant read. Image parts are not defined in
this revision; the capability document's `input_parts` lists what a server
accepts.

The optional `limits` object imposes core-owned budgets on the run:

```json
{
  "type": "submit_prompt",
  "session_id": "...",
  "input": [{ "type": "text", "text": "Refactor the parser." }],
  "limits": {
    "max_duration_ms": 600000,
    "max_model_turns": 40,
    "max_tool_calls": 200,
    "max_total_tokens": 2000000,
    "max_cost_usd_nanos": 2000000000,
    "max_input_tokens": 1500000,
    "max_output_tokens": 500000,
    "max_tool_output_bytes": 4000000,
    "max_children": 4,
    "max_concurrent_children": 2
  },
  "correlation": { "job": "j-1" }
}
```

Every field is optional and must be greater than zero when present; an
unknown field or a zero value is rejected as an invalid request.
`max_children` and `max_concurrent_children` must not exceed the runtime
ceilings the capability document advertises (8 and 3); they lower the bound
for this run, and a refused spawn is reported to the model as a tool error
rather than settling the run. Sub-agent depth is fixed at 1 and advertised,
not accepted as a limit. `max_input_tokens` counts fresh plus cached input;
`max_output_tokens` counts output; both settle with their own kind, and a
provider turn that omits usage under any token bound settles as
`tokens_unknown`. `max_tool_output_bytes` counts tool results as the model
receives them (after per-result truncation). `correlation` is stored on the
run and echoed on `RunSnapshot`. Limits are
persisted with the run and enforced by the runtime, not the client, so every
surface observes the same outcome. The wall clock starts at admission and
spans provider retries, tool execution, and sub-agent work. When the turn or
tool-call budget is nearly spent, the runtime reserves the last permitted turn
as a tool-free final status response. `max_cost_usd_nanos` requires the
resolved model to carry pricing; otherwise the run fails with a
`configuration` failure before any provider work. Sub-agents receive the
parent's *remaining* wall clock, cost, and token bounds at spawn time (never
the original caps, and never the parent's per-run turn, tool-call, byte, or
child counts); a family the parent has already spent refuses the spawn as a
tool error naming it. Their settled usage and cost are charged to the parent's
meter, so every `*_tokens` and cost bound covers the whole subtree.

Outcome on accept:

```json
{
  "type": "prompt_queued",
  "session_id": "...",
  "run_id": "...",
  "queue_position": 0
}
```

Emits `prompt_queued`, then later run lifecycle events as the runtime executes
the queue.

### `POST /v1/runs/cancel`

```json
{
  "command_id": "...",
  "command": {
    "type": "cancel_run",
    "run_id": "..."
  }
}
```

Outcomes:

```json
{ "type": "cancellation_requested", "run_id": "..." }
```

```json
{
  "type": "run_already_finished",
  "run_id": "...",
  "outcome": { "type": "completed" }
}
```

A live cancel emits `cancellation_requested` and eventually `run_finished`
with a cancelled/interrupted outcome once the runtime stops the work. Steering
still queued for the run is superseded (`steering_superseded`) in the same
transaction that settles it.

### `POST /v1/runs/steer`

```json
{
  "command_id": "...",
  "command": {
    "type": "steer_run",
    "run_id": "...",
    "input": [{ "type": "text", "text": "also check the tests" }],
    "interrupt": false
  }
}
```

Adds user input to a run that is already executing. Four operations on a live
run are distinct:

| Intent | Command | Effect |
| --- | --- | --- |
| Queue the next prompt | `submit_prompt` | A new run after this one finishes |
| Steer at the next boundary | `steer_run` (`interrupt: false`) | Input joins the current run's context after the turn in flight completes and its tool results are appended |
| Interrupt and steer | `steer_run` (`interrupt: true`) | The in-flight provider stream, approval wait, or tool execution is aborted so the boundary arrives now, then the input joins |
| Cancel | `cancel_run` | Durable terminal `cancelled` outcome |

`input` obeys the same bounds as `submit_prompt` (workspace file parts are
rendered as placeholders; steering attaches no files). At most 4 steering
messages may be pending per run; the fifth is a `400`. Only a `running`
prompt run can be steered: a queued run (`400`, not yet started; submit a
prompt or cancel instead), a compaction run, or an unknown run is refused.
Steering never rewrites a provider request already known to be possibly
delivered: when the model returns no tool calls but steering is pending, the
run continues with the steering instead of completing.

With `interrupt`, text the model already streamed is kept as the partial
assistant turn; tool calls it had begun are dropped (nothing executed), tool
calls awaiting approval or executing settle as `interrupted` with an error
result, and `run_interrupted` is published. If no steering is pending when the
boundary arrives (the client raced a finishing turn), the run continues with a
runtime notice so the transcript stays provider-valid.

Outcomes:

```json
{ "type": "steering_queued", "run_id": "...", "message_id": "..." }
```

```json
{ "type": "run_already_finished", "run_id": "...", "outcome": { "type": "completed" } }
```

The `message_id` names a user `MessageSnapshot` with `steering: true`,
published on `steering_queued` in state `queued`. `steering_applied` moves it
to `complete` and reports the `turn_ordinal` whose request first carried it;
`steering_superseded` moves it to `cancelled` when the run settled first.
Replaying the command returns the same receipt without queuing again.

### `POST /v1/tools/approvals`

```json
{
  "command_id": "...",
  "command": {
    "type": "respond_tool_approval",
    "run_id": "...",
    "tool_call_id": "...",
    "decision": { "type": "approve_once" }
  }
}
```

Decision variants:

| `decision.type` | Meaning |
| --- | --- |
| `approve_once` | Run this call only |
| `approve_for_session` | Run this call and record a session grant |
| `approve_for_workspace` | Like `approve_for_session`, plus promote the grant into workspace configuration |
| `deny` | Reject the call |

`approve_for_session` and `approve_for_workspace` include a grant:

```json
{ "type": "approve_for_session", "grant": { "type": "tool", "name": "edit_file" } }
```

```json
{
  "type": "approve_for_workspace",
  "grant": { "type": "shell_prefix", "prefix": "cargo test" }
}
```

Outcome:

```json
{
  "type": "tool_approval_resolved",
  "tool_call_id": "...",
  "resolution": "approved_once"
}
```

Resolution values: `approved_once`, `approved_for_session`,
`approved_for_workspace`, `approved_by_reviewer`, `denied`, `denied_timeout`,
`denied_by_reviewer`. The reviewer resolutions are written by the configured
`reviewer_model` without a human: `approved_by_reviewer` for `auto` sessions'
dangerous shell and for every held call of a `supervised` child;
`denied_by_reviewer` only for `supervised` children, where the denial is final
and the child receives it as a tool error. Reviewer spend is charged to the
reviewed run.

Approval modes are `read_only`, `supervised`, `ask`, `auto` (default), and
`full`. `supervised` is never advertised for root sessions and cannot be set by
`set_approval_mode`: it is the mode a `spawn_agent` call with
`authority: "write"` gives its child, and a spawned child's mode may only be
lowered by a client, never raised (`child_authority_escalation`).

`approve_for_workspace` resolves the approval exactly like
`approve_for_session` — the session grant is recorded in the same
transaction, so the waiting run proceeds immediately — and additionally
requests that the grant be written into the workspace's `.qq/config.ron`
policy section. That durable write happens after the approval commits and
is first recorded in a bounded SQLite outbox in the same transaction. One
serial worker drains that outbox and atomically removes each entry with its
separate `workspace_grant_promoted` fate event, carrying one of:

```json
{ "type": "written", "path": "/repo/.qq/config.ron" }
{ "type": "already_present", "path": "/repo/.qq/config.ron" }
{ "type": "failed", "message": "..." }
```

A `failed` outcome (managed-layer deny, IO error) is informational only:
the approval stands and the session grant remains in force. Retrying the
`respond_tool_approval` command with the same `command_id` replays the
original receipt and does not enqueue a duplicate promotion. If the process
stops after accepting the approval, startup resumes the existing outbox entry;
the configuration write is idempotent, so recovery normally observes
`already_present` when the file write landed before the interruption.

### `POST /v1/sessions/approval-mode`

```json
{
  "command_id": "...",
  "command": {
    "type": "set_approval_mode",
    "session_id": "...",
    "mode": "auto"
  }
}
```

Modes:

| Mode | Behavior |
| --- | --- |
| `read_only` | Deny mutating, shell, and MCP tools without prompting |
| `ask` | Prompt for mutating/shell/MCP unless granted |
| `auto` | Default. Edits and safe shell run without prompting; only dangerous shell (deletion, privilege escalation, force-push, piped installers) is held |
| `full` | Every tool call executes without prompting |

The mode is read when each approval is evaluated, so it applies to the next
held call even during an active run. The command commits and publishes a
`session_updated` event whose summary carries the new `approval_mode`.

Outcome:

```json
{
  "type": "approval_mode_set",
  "session_id": "...",
  "mode": "auto"
}
```

### `POST /v1/sessions/model`

```json
{
  "command_id": "...",
  "command": {
    "type": "set_session_model",
    "session_id": "...",
    "model": {
      "model": "provider/model-id",
      "max_output_tokens": 8192,
      "organization": null
    }
  }
}
```

Repoints the session's model. The selection is validated exactly like
`create_session`. It takes effect when the next run is claimed; a run that is
already executing keeps the model it started with.

Outcome:

```json
{
  "type": "session_model_set",
  "session_id": "...",
  "model": { "model": "provider/model-id" }
}
```

Emits `session_updated` carrying the full refreshed `SessionSummary`.

### `POST /v1/sessions/profile`

```json
{
  "command_id": "...",
  "command": {
    "type": "set_session_profile",
    "session_id": "...",
    "profile": "fast"
  }
}
```

Repoints the session's agent profile. Takes effect when the next run is
claimed; an executing run keeps the plan it started with. The name must be
well-formed; whether the workspace configuration declares it is decided at
claim time, where an unknown profile fails that run with a `configuration`
failure.

Outcome:

```json
{ "type": "session_profile_set", "session_id": "...", "profile": "fast" }
```

Emits `session_updated`.

### `POST /v1/sessions/delete`

```json
{
  "command_id": "...",
  "command": {
    "type": "delete_session",
    "session_id": "..."
  }
}
```

Deletes the session and every row it owns — runs, messages, model turns, tool
calls, session files, and grants — in one transaction. Rejected with `400`
while the session has an active run; cancel the run first. Child sessions
survive as roots.

The session's rows in the workspace event log are deliberately **kept**:
cursors promise a gapless `previous + 1` sequence to subscribers, so removing
event rows would break every replay that spans the deletion. Replaying the
kept events is harmless because the trailing `session_deleted` event
converges any client on the deleted state; snapshots never include deleted
sessions.

Outcome:

```json
{
  "type": "session_deleted",
  "session_id": "..."
}
```

Emits `session_deleted`.

### `POST /v1/sessions/prune`

```json
{
  "command_id": "...",
  "command": {
    "type": "prune_sessions",
    "workspace_id": "..."
  }
}
```

Deletes every idle session in the workspace that has no messages and no runs
(the residue of creating sessions without prompting them), with the same
per-session guarantees as `delete_session`.

Outcome:

```json
{
  "type": "sessions_pruned",
  "workspace_id": "...",
  "deleted": 2
}
```

Emits one `session_deleted` per deleted session.

### `POST /v1/sessions/compact`

```json
{
  "command_id": "...",
  "command": {
    "type": "compact_session",
    "session_id": "..."
  }
}
```

Compacts the session's model context. Valid only while the session is idle:
rejected with `400` while a run is active or prompts are queued, exactly like
`delete_session`'s active-run refusal. The command queues an **internal**
summarization run that flows through the ordinary run machinery (permits,
cancellation via `cancel_run`, usage and cost accounting) but whose request
messages and streamed output never join the session transcript. Its product
is a durable summary row plus a cutoff marker, committed atomically with the
run's completion; later runs assemble context as agent instructions + latest
summary + verbatim transcript after the marker. The client transcript is
untouched. A crash mid-summarization commits no marker; retry the command.

A later `compact_session` summarizes the current summary together with the
span since the marker, so repeated compactions fold rather than stack. A
small bounded history of prior compactions (three rows) is retained
server-side for rollback.

The summary is validated before it commits: it must be non-empty, fit the
session context limit, carry every required section heading (Intent;
Decisions and constraints; Work state; Files touched; Errors; User messages),
and shrink the assembled context relative to the prior assembly once that
assembly exceeds a small floor. A summary failing any check fails the run
with a `policy` failure and leaves the prior compaction (or the verbatim
transcript) in force.

Outcome:

```json
{
  "type": "compaction_queued",
  "session_id": "...",
  "run_id": "..."
}
```

Emits `session_updated` (the session shows queued), then the internal run's
`run_started` and `run_finished`, then `session_compacted` when the summary
commits. A failed or cancelled summarization emits only `run_finished` with
that outcome and leaves assembly unchanged.

Runs inside a compacted session may call the built-in read-only
`search_history` tool, which searches the complete durable transcript
(including spans compaction replaced) and returns bounded, cited excerpts.

### `POST /v1/sessions/compact/rollback`

```json
{
  "command_id": "...",
  "command": {
    "type": "rollback_compaction",
    "session_id": "..."
  }
}
```

Discards the session's most recent compaction so the next run assembles from
the previous retained compaction, or from the verbatim transcript when none
remains. Valid only while the session is idle (`400` otherwise); `400` when
the session has no compaction to roll back. Context occupancy is cleared
because the next assembly is not yet known.

Outcome:

```json
{
  "type": "compaction_rolled_back",
  "session_id": "...",
  "remaining": 1
}
```

Emits `session_compaction_rolled_back` with the refreshed `SessionSummary`
and the count of retained compactions still available to roll back.

### `POST /v1/workspaces/snapshot`

Not a command. Request:

```json
{
  "workspace_id": "...",
  "focused_session_id": null,
  "include_sessions": [],
  "session_limit": 512,
  "message_limit": 256
}
```

| Field | Rules |
| --- | --- |
| `session_limit` | Required, `1..=512` |
| `message_limit` | Required, `1..=256` in current runtime bounds |
| `focused_session_id` | Optional; when set, include full session detail |
| `include_sessions` | Optional, at most 16 ids; bodies for each are returned in `included` (request order). Ids equal to `focused_session_id`, outside the workspace, or unknown are skipped rather than rejected |

Response `WorkspaceSnapshot`:

```json
{
  "cursor": { "store_id": "...", "workspace_id": "...", "sequence": 12 },
  "workspace": { "id": "...", "path": "/path/to/repo" },
  "sessions": [ /* SessionSummary, newest first */ ],
  "focused": {
    "summary": { /* SessionSummary */ },
    "messages": [ /* MessageSnapshot */ ],
    "runs": [ /* RunSnapshot */ ],
    "tool_calls": [ /* ToolCallSnapshot */ ],
    "has_older_tool_calls": false,
    "has_older_messages": false
  },
  "included": [ /* SessionSnapshot per include_sessions entry, omitted when empty */ ],
  "has_older_sessions": false
}
```

Snapshots are the catch-up mechanism after connect or cursor loss. Live SSE
then advances the client from `cursor`.

### `POST /v1/models`

Model catalog lookup for a workspace and selection hint.

Request:

```json
{
  "workspace": "/path/to/repo",
  "selection": {
    "model": null,
    "max_output_tokens": null,
    "organization": null
  }
}
```

Response: JSON array of `ModelDescriptor`:

```json
[
  {
    "provider": "openai",
    "model": "gpt-5",
    "name": "GPT-5",
    "context_window": 128000,
    "selection": {
      "model": "openai/gpt-5",
      "max_output_tokens": null,
      "organization": null
    }
  }
]
```

### `GET /v1/workspaces/{workspace_id}/events`

Resumable SSE subscription for one workspace.

Required header:

```http
Last-Event-ID: <store_id>:<workspace_id>:<sequence>
Accept: text/event-stream
```

`Last-Event-ID` is mandatory. Clients start from the snapshot cursor (or a
previous event id). The server:

1. Validates the cursor's store and workspace.
2. Replays every persisted event with `sequence > after.sequence`.
3. Switches to live delivery of newly committed events.
4. Sends keep-alive comments every 15 seconds while idle.

Each data event:

```text
id: <store_id>:<workspace_id>:<sequence>
event: session_event
data: { ...SessionEventEnvelope... }
```

Client validation expectations:

- SSE `id` equals `envelope.cursor` rendered in wire form.
- `cursor.sequence` is exactly previous sequence + 1.
- `cursor.workspace_id` matches the subscribed workspace.
- `cursor.store_id` remains stable for the life of the stream.

If the stream gaps, the client must resnapshot and reconnect. Do not invent
events to fill holes.

## Session Event Envelope

Every streamed payload is a `SessionEventEnvelope`:

```json
{
  "cursor": {
    "store_id": "...",
    "workspace_id": "...",
    "sequence": 7
  },
  "session_id": "...",
  "run_id": "...",
  "caused_by": "...",
  "occurred_at_ms": 1710000000123,
  "event": {
    "type": "text_appended",
    "message_id": "...",
    "channel": "output",
    "text": "hello"
  }
}
```

| Field | Presence | Meaning |
| --- | --- | --- |
| `cursor` | always | Durable workspace position |
| `session_id` | always | Session the event belongs to |
| `run_id` | optional | Set when the event is run-scoped |
| `caused_by` | optional | `command_id` that produced the event |
| `occurred_at_ms` | always | Server Unix time in milliseconds |
| `event` | always | Tagged `SessionEvent` body |

### `SessionEvent` variants

| `type` | Principal fields | When |
| --- | --- | --- |
| `session_created` | `session` | New session row committed |
| `session_updated` | `session` | Non-run session mutation (model repointed, compaction queued) |
| `session_deleted` | `session_id` | Session and its rows deleted; earlier events remain |
| `prompt_queued` | `session`, `message`, `run`, `queue_position` | User prompt accepted |
| `run_started` | `session`, `run_id`, optional `plan` | Run leaves the queue; `plan` is its fixed `RunPlanIdentity` |
| `steering_queued` | `run_id`, `message` | Steering input durably recorded (message `steering: true`, state `queued`) |
| `steering_applied` | `run_id`, `message_id`, `turn_ordinal` | Steering entered model context for that turn |
| `steering_superseded` | `run_id`, `message_id` | Run finished before the steering applied |
| `run_interrupted` | `run_id`, `turn_ordinal` | An interrupting steer aborted the turn in flight |
| `run_output_truncated` | `run_id`, `turn_ordinal`, `continuation` | The provider cut the turn at its output token limit; the partial turn is committed and the run resumes on the next turn |
| `run_audit_started` | `run_id`, `audit_session_id` | A read-only audit child is checking the run's candidate final answer |
| `run_audit_completed` | `run_id`, `audit` | The audit settled (`pass`, `revised`, or `unavailable`); on `revised` the run continues once with the findings |
| `assistant_message_started` | `message` | A model turn's message begins streaming |
| `text_appended` | `message_id`, `channel`, `text` | Output or refusal delta |
| `model_turn_completed` | `run_id`, `turn_ordinal`, `model`, optional `usage`, optional `estimated_cost_usd_nanos` | A provider inference and its accounting committed |
| `tool_call_requested` | `tool_call` | Model finished requesting a tool call |
| `tool_approval_requested` | `tool_call`, optional `shell`, optional `edit` | Policy needs a human decision |
| `tool_approval_resolved` | `tool_call`, `resolution` | Approval decision recorded |
| `workspace_grant_promoted` | `grant`, `outcome` | An approve-for-workspace promotion finished (`written`, `already_present`, or non-fatal `failed`) |
| `tool_call_started` | `tool_call` | Execution began |
| `tool_call_output_delta` | `tool_call_id`, `chunk` | Incremental output from a running call (shell) |
| `tool_call_finished` | `tool_call` | Execution ended with result/error |
| `run_context_updated` | `run_id`, `context_tokens` | A measured model turn committed; the run audit value moved |
| `session_context_updated` | `run_id`, optional `context_tokens` | A current-model prompt turn committed; the session meter moved or became unknown |
| `cancellation_requested` | `session`, `run_id` | Cancel command accepted for a live run |
| `run_finished` | `session`, `run_id`, `outcome`, optional `usage`, optional `context_tokens` | Terminal run state |
| `session_compacted` | `session`, optional `summary`, `before_bytes`, `after_bytes` | Compaction summary + cutoff committed |
| `session_compaction_rolled_back` | `session`, `remaining` | Latest compaction discarded; `remaining` prior compactions still roll back |

Text channels:

- `output` — normal assistant text
- `refusal` — model refusal text

`session_compacted` carries the refreshed `SessionSummary` (the internal run
has already finished when it is published), a bounded excerpt of the summary
text (optional on the wire), and the assembled context size in bytes before
and after the compaction so clients can surface the shrink without waiting
for the next run's usage. The summary's `context_tokens` is absent because the
compaction provider usage measured the replaced input, not the new summary.

### Snapshots embedded in events

Events often carry denormalized snapshots so clients can render without a
round trip:

**`SessionSummary`**

```json
{
  "id": "...",
  "workspace_id": "...",
  "parent_id": null,
  "spawned_by": null,
  "title": "New session",
  "status": "running",
  "active_run_id": "...",
  "activity": "generating_response",
  "queued_prompts": 0,
  "model": "provider/model",
  "context_tokens": 12500,
  "estimated_cost_usd_nanos": 1234567,
  "updated_at_ms": 1710000000123,
  "last_outcome": null
}
```

Session status: `idle`, `queued`, `running`.

`spawned_by` is set on children created by a parent run's `spawn_agent` call:
`{ "run_id": "...", "tool_call_id": "..." }`. `tool_call_id` is absent for
children persisted before the call was recorded. `activity` mirrors the latest
`run_activity_changed` for `active_run_id` and is absent when idle or unknown,
so a client that loads mid-run shows the right label without waiting for the
next event.

`context_tokens` is the latest exact prompt-turn input total measured for the
session. It is absent when unknown. A successful compaction or a model change
clears it until another prompt turn reports usage; clients must not reconstruct
it from cumulative run billing.

**`RunSnapshot`**

```json
{
  "id": "...",
  "session_id": "...",
  "status": "running",
  "outcome": null,
  "prompt_identity": {
    "version": 7,
    "instruction_hash": "1111111111111111111111111111111111111111111111111111111111111111",
    "system_prompt_hash": "2222222222222222222222222222222222222222222222222222222222222222",
    "tool_schema_hash": "3333333333333333333333333333333333333333333333333333333333333333",
    "selected_guidance": {
      "kind": "skill",
      "name": "review",
      "source": ".qq/skills/review/SKILL.md",
      "content_hash": "4444444444444444444444444444444444444444444444444444444444444444"
    }
  },
  "resolved_model": {
    "version": 2,
    "request_shape": {
      "version": 1,
      "digest": "5555555555555555555555555555555555555555555555555555555555555555"
    },
    "route": "xai/grok-4.5",
    "provider_model": "grok-4.5",
    "organization": "example-org",
    "credential_profile": "work",
    "max_output_tokens": 4096,
    "context_window": 128000,
    "pricing": {
      "input_usd_nanos_per_token": 1250,
      "output_usd_nanos_per_token": 10000,
      "cache_read_usd_nanos_per_token": 125,
      "provenance": "built-in catalog"
    },
    "output_token_control": "native",
    "generation": {
      "reasoning_effort": "unsupported"
    },
    "prompt_cache": {
      "control": "unsupported",
      "cache_read_usage": true,
      "cache_write_usage": false
    }
  },
  "usage": null,
  "estimated_cost_usd_nanos": null,
  "context_tokens": null,
  "limits": { "max_model_turns": 40 }
}
```

Run status: `queued`, `running`, `completed`, `cancelled`, `failed`,
`interrupted`, `budget_exhausted`.

`limits` echoes the caller-imposed budgets the run was admitted under and is
omitted for runs submitted without any.

`prompt_identity.version` identifies the shared provider-neutral system prompt
contract prepared for the run. `instruction_hash` identifies the selected root
instructions: `AGENTS.md`, `CLAUDE.md` when `AGENTS.md` is absent, or the empty
selection. `system_prompt_hash` identifies the exact prepared system text, and
`tool_schema_hash` identifies the ordered provider tool schemas. When a slash
command or skill was selected, `selected_guidance` records its kind, name,
source, optional declared version, and content hash. Nested instructions found
later remain durable tool evidence but do not retroactively change this
pre-provider identity. `prompt_identity` is absent for historical runs and
runs that failed before prompt preparation; version-6 rows may omit the fields
added in version 7. `catalog_digest` and `exposure` identify the compiled tool
catalog the run was admitted with and how it was exposed; `context_sources`
lists every attached `ContextSource` with its outcome (`fetched`,
`fetched_truncated`, `cached`, `cached_truncated`, `timed_out`, `unavailable`,
`refused`, `invalid`), item and byte counts, and the content hash of the block
appended to the system prompt. All three are absent or empty on rows written
before version 14.

`resolved_model` is the versioned, secret-free execution descriptor committed
once after runtime resolution and before the provider stream is polled. Its
`route` is the effective QQ selection, while `provider_model` is the exact
identifier placed in provider-neutral requests. The output cap is the minimum
of the configured cap and known model metadata; unknown model metadata leaves
the configured cap unchanged. Optional organization and named credential
profile identify the selected non-secret routing/auth context. Credential
values, API keys, access tokens, and secret hashes are never represented.
Pricing retains its provenance, and the capability fields describe controls
and cache-usage accounting that the selected codec actually implements.
Version-2 descriptors may carry `request_shape`, an opaque, versioned digest of
the exact secret-free provider adapter/API/deployment shape compiled by the
root. It is absent for historical version-1 descriptors and whenever an exact
identity would require reading secret-bearing endpoint userinfo/query/fragment
or custom static headers. Custom and LiteLLM endpoint configurations always
omit it because arbitrary URL paths can themselves carry credentials. Consumers
must treat absence as unknown and disable cross-run compatibility reuse; the
digest is not a credential fingerprint. AWS deployments that resolve their
region dynamically likewise omit it because the effective region is not stable
across restart.
`unsupported` output control (currently Codex Responses) means QQ still bounds
its provider-neutral request and response processing to
`max_output_tokens`, but the codec deliberately omits a provider-side output
parameter. Historical runs and runs that fail before model resolution omit the
descriptor rather than borrowing current configuration.

`usage` sums every model turn in the run and is the billing figure;
`context_tokens` is the final completed turn's input-token total (fresh
input + cache reads + cache writes) for that run. Internal compaction runs
measure the pre-compaction summarizer request, so this per-run audit field is
not a substitute for `SessionSummary.context_tokens`.
`model_turn_completed` is persisted before any tool dispatch and records every
provider inference, including tool-only and internal compaction turns, so
trajectory exporters do not have to infer per-turn model or accounting data.
Its `model` is the effective route, organization, and output cap from the run's
descriptor. The event's `run_id` links to the single run-level descriptor;
pricing and capability metadata are not duplicated into every turn event.
`run_context_updated` streams measured per-run audit values and is absent for
unmeasured turns and runs persisted before version 3.
`session_context_updated` is the live session-meter event. It is emitted only
for prompt turns whose model still matches the session's selected model; an
absent `context_tokens` explicitly clears the meter when that turn was not
measured. Clients must ignore `run_context_updated` for session occupancy,
including during replay of pre-version-5 events. Both events carry no
snapshots. Internally, the server reuses a measured session value across runs
only when its persisted request shape and static system/tool prefix match
exactly and the newly measured request bytes grow monotonically. Pricing-only
descriptor refreshes remain compatible; model, codec, endpoint, organization,
generation/output, system-prompt, or tool-schema changes do not.
Context assembly that replaces stale tool results with pruning stubs also
disables reuse because total byte growth alone cannot prove append-only history.

**`MessageSnapshot`**

```json
{
  "id": "...",
  "session_id": "...",
  "run_id": "...",
  "role": "assistant",
  "state": "streaming",
  "turn_ordinal": 1,
  "output": "partial text",
  "refusal": "",
  "created_at_ms": 1710000000123
}
```

Roles: `user`, `assistant`.  
Message state: `queued`, `streaming`, `complete`, `cancelled`, `failed`,
`interrupted`.

The unit of assistant output is the model turn: each turn that produces
text gets its own message, `turn_ordinal` 1-based and matching the
ordinals on that turn's tool calls, so clients can render text and calls
in execution order. User messages and rows persisted before per-turn
messages use `turn_ordinal` 0.

**`ToolCallSnapshot`**

```json
{
  "id": "...",
  "session_id": "...",
  "run_id": "...",
  "turn_ordinal": 1,
  "call_ordinal": 0,
  "provider_call_id": "call_...",
  "name": "read_file",
  "arguments": "{\"path\":\"README.md\"}",
  "state": "completed",
  "result": "# QQ\n...",
  "is_error": false,
  "display": { "type": "diff", "path": "src/lib.rs", "diff": "- old\n+ new\n" }
}
```

Tool state: `requested`, `awaiting_approval`, `running`, `completed`,
`failed`, `denied`, `interrupted`.

`display` is an optional, extensible tagged payload for client rendering
only (first variant: `diff`, carried by successful `edit_file` and
`write_file` calls). It is absent unless populated, never enters model
context, and the `result` string remains authoritative.

### Approval previews

`tool_approval_requested` may include one of:

```json
{
  "shell": {
    "command": "cargo test -p qq-core",
    "cwd": "crates/qq-core"
  }
}
```

```json
{
  "edit": {
    "path": "src/lib.rs",
    "diff": "- old\n+ new\n"
  }
}
```

Previews are advisory UI aids. The authoritative call remains `tool_call`.

### Run outcomes and failures

```json
{ "type": "completed" }
{ "type": "cancelled" }
{ "type": "interrupted" }
{
  "type": "failed",
  "failure": {
    "kind": "provider_rate_limited",
    "message": "..."
  }
}
{
  "type": "budget_exhausted",
  "exhaustion": {
    "limit": "model_turns",
    "final_response": true,
    "message": "the run exhausted its 40 model turn budget"
  }
}
```

`budget_exhausted` settles a run whose caller-imposed `limits` ran out. It is
never a `failed` outcome: the harness, model, and provider behaved. The run
status is also `budget_exhausted`. `final_response` reports whether the
reserved tool-free status turn was granted; it is `false` when the wall clock
elapsed, when cost became unmeasurable, or when the model requested a tool on
the final turn.

`BudgetLimitKind` values:

```text
duration
model_turns
tool_calls
total_tokens
cost
cost_unknown
input_tokens
output_tokens
tokens_unknown
tool_output_bytes
```

`cost_unknown` means a cost cap was imposed but a provider turn (of the run or
of a sub-agent) omitted usage, so spend could no longer be measured.
`tokens_unknown` is the same signal for any token bound (`max_total_tokens`,
`max_input_tokens`, `max_output_tokens`): a caller must never believe a token
bound held when the provider stopped reporting usage.

### Run plan identity

```json
{
  "profile": "review",
  "descriptor_version": 3,
  "digest": "aaaa…",
  "credential_epoch": 3
}
```

`RunPlanIdentity` is fixed when a run starts and carried on `run_started` and
`RunSnapshot.plan`. `digest` is the SHA-256 of the secret-free
`AgentPlanDescriptor` the run was compiled from (provider shape, model,
workspace, prompt version, instructions, tool catalog with its host
generations and exclusions, skill index, agent pack, MCP declarations, retry
policy, configuration sources, profile); two runs with equal digests were
admitted with behaviorally identical plans. `credential_epoch` is the opaque
credential-store generation that authorized the plan; it moves on rotation
without changing the digest. A later configuration or credential refresh
compiles a new plan for later runs and never changes an accepted run's
identity. Historical runs and runs that failed before compilation carry no
`plan`. The descriptor itself is persisted beside the run but is not on the
wire.

`RunFailureKind` values:

```text
invalid_command
configuration
authentication
policy
server
provider_configuration
provider_authentication
provider_rate_limited
provider_invalid_request
provider_unavailable
provider_transport
provider_api
provider_response
provider_protocol
context_source
provider_output_truncated
```

`context_source` settles a run whose fail-closed `ContextSource` did not
deliver before any provider work; the record on `prompt_identity` names the
source and outcome.

`provider_output_truncated` settles a run whose provider stopped at its output
token limit on more consecutive turns than the runtime continues
(`max_output_continuations`, currently 3). Each truncated turn is committed as
a `truncated` assistant message and is charged to the run; the failure message
names the cap and the turn count so the reason is never a generic "response
was incomplete". Content-filter and refusal stops remain `provider_response`
and are never continued.

### Token usage

```json
{
  "input_tokens": 1000,
  "cache_read_input_tokens": 200,
  "cache_write_input_tokens": 0,
  "output_tokens": 250,
  "reasoning_tokens": 90
}
```

`output_tokens` is every generated token and is what budgets and billing use.
`reasoning_tokens` is the part of that total the provider reported as hidden
reasoning (OpenAI `reasoning_tokens`, Google `thoughtsTokenCount`); it is
omitted when the provider does not break it out (Anthropic, Bedrock) and is
never added to `output_tokens` again. Aggregates keep it only while every
summed turn reported one. Added in protocol 16 as an optional field; older
encodings decode unchanged.

Usage may appear on `run_finished`. Estimated costs on summaries are integer
US-dollar nanos (`1e-9 USD`) when pricing data is available.

## Idempotency

Mutating commands are durable-idempotent:

1. Client generates a fresh `command_id` per logical user action.
2. Server stores `(command_id, request_json, receipt_json)`.
3. A retry with the same id and identical request JSON returns the original
   `CommandReceipt` without re-executing side effects.
4. A retry with the same id and different request JSON fails as an
   idempotency conflict (surfaced as a rejected request).

Clients must not reuse a `command_id` for a different action. After a transport
failure with an unknown result, retry the **same** id and body, then reconcile
through snapshot/SSE rather than submitting a second logical command.

## Client Lifecycle

Recommended attach sequence for an interactive client:

```text
1. Discover local server metadata (or start qq serve / embedded server).
2. GET /v1/health and verify protocol_version.
3. POST resolve_workspace for the canonical workspace path.
4. POST workspaces/snapshot with desired page limits.
5. GET workspaces/{id}/events with Last-Event-ID = snapshot.cursor.
6. Apply live SessionEventEnvelope messages in order.
7. POST commands for user actions; match caused_by / command receipts.
8. On stream gap, 401 after restart, or invalid cursor: resnapshot and resume.
```

Direct CLI automation (`qq ask`) currently executes through the in-process
runtime rather than the session HTTP API. The session protocol above is the
supported multi-client surface.

## Direct Runtime Types

`qq-protocol` also exports a smaller vocabulary used by the in-process run
path (`qq ask` and internal runtime tests):

- `RunCommand { prompt }`
- `RunEvent` — `started`, `output_text_delta`, `refusal_delta`, `usage`,
  `completed`, `failed`

These types are not persisted session state. `qq ask` consumes them entirely
in-process; use the workspace/session routes or `qq run` for work that must be
durable, resumable, approval-capable, or shared across clients.

## Size And Validation Bounds

Current server/client bounds that affect interoperability:

| Limit | Value |
| --- | --- |
| HTTP request body | 1 MiB |
| SSE event JSON | 1 MiB |
| Snapshot response (client read limit) | 8 MiB |
| Model catalog response | 2 MiB |
| Health response | 16 KiB |
| Workspace path field | 4 KiB |
| Model id field | 512 bytes |
| Organization field | 512 bytes |
| Capabilities response (client read limit) | 256 KiB |
| Input parts per prompt or steer | 1..=32 |
| Input text per prompt or steer | 128 KiB |
| Workspace file parts per prompt | 8, each ≤ 256 KiB, resolved total ≤ 1 MiB |
| Pending steering per run | 4 |
| Correlation map | 8 entries, key ≤ 64 B, value ≤ 256 B, ≤ 2 KiB |
| Agent profile id | 64 bytes |
| Snapshot `session_limit` | 1..=512 |
| Bootstrap snapshot used by the shipped TUI client | 512 sessions / 256 messages |

Field-level validation rejects empty prompts/paths where applicable and
unknown fields on structured request bodies.

## Compatibility Rules

When changing the protocol:

1. Update types in `qq-protocol` first.
2. Add/adjust contract tests that lock tag names and field shapes.
3. Bump `PROTOCOL_VERSION` for breaking wire changes.
4. Update this document in the same change.
5. Keep provider-specific codecs out of this protocol; provider wire formats
   belong in `qq-provider`.

Additive optional fields may be introduced carefully with serde defaults, but
clients generated against older structs will ignore unknown response fields
only if their decoders allow it. The server continues to reject unknown fields
on inbound request bodies that opt into `deny_unknown_fields`. Two response
types deliberately tolerate unknown fields: `ServerInfo` and
`ServerCapabilities` (with its sections), so an older client can read a newer
server's version and report the skew. Events, snapshots, and every inbound
type stay strict.

Golden encodings for every command, receipt, event, and the capability
document live under `crates/qq-protocol/tests/fixtures/v15/` and are checked
byte-for-byte by `crates/qq-protocol/tests/wire_fixtures.rs`. A wire change
fails that test first; regenerate the goldens with `QQ_UPDATE_FIXTURES=1`
after bumping `PROTOCOL_VERSION`.

## Non-Goals

The current protocol does not define:

- Multi-user identity or ACLs beyond the single local bearer token
- Per-session SSE streams (streams are workspace-scoped)
- Binary attachment upload APIs (workspace files attach by reference; image
  parts are not yet defined)
- Interactive PTY multiplexing
- Client-to-client messaging
- Partial event patch frames (events are whole JSON objects)

Those features require an explicit protocol revision.

## Final-answer audit

When the configured `audit` section enables it (`mode: heuristic` by default:
the run mutated a file, ran a non-read shell command, made twelve or more tool
calls, or spawned a child; `always`; or `off`), a root run's candidate final
answer is handed to a read-only child session (`purpose: "audit"`) at the
roster's configured role before the run completes. The child receives the user
prompt, the answer, and a bounded list of the run's tool calls, verifies the
answer's claims against the workspace with its own tools, and replies with one
JSON verdict. `run_audit_started` names the audit session on the parent run
atomically with its creation; `run_audit_completed` carries the durable
`AuditRecord`:

```json
{
  "outcome": "revised",
  "findings": ["src/auth.rs still redirects to /login"],
  "revisions": 0,
  "usage": { "input_tokens": 900, "cache_read_input_tokens": 0,
             "cache_write_input_tokens": 0, "output_tokens": 40 },
  "estimated_cost_usd_nanos": 1200
}
```

`pass` completes the run. `revised` sends the run back once (bounded by
`max_revisions`, at most 2) with the findings as a runtime notice; the revised
answer stands unaudited when the cap is reached. `unavailable` (the auditor
failed, was refused, or answered prose) is fail-open: the answer completes and
the record says so. The audit child's usage and cost are charged to the audited
run; the latest record is on `RunSnapshot.audit`. Child runs, internal runs,
budget-final turns, and runs whose remaining budget cannot fund an auditor are
never audited.
