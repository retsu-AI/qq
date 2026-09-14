# Headless Contract And Hosting Boundary

Status: design document, 2026-09-05. Describes the contract an external
supervisor consumes when it runs `qq` non-interactively, and the boundary
between what belongs in this repository and what belongs to a product that
hosts it. Companion to [`architecture.md`](./architecture.md), which owns crate
boundaries, and [`protocol.md`](./protocol.md), which owns the HTTP/SSE wire
contract. Where this document describes current behavior it cites source; where
it describes intended additions it labels them as such and points at the
owning plan task.

## Why This Document Exists

QQ has two kinds of consumer:

1. A person at a terminal (TUI, `qq ask`, `qq run` in a shell).
2. A **supervisor**: a batch runner, CI job, evaluation harness, or hosted
   service that launches `qq` inside an environment it controls, observes the
   run through structured output, and decides what to do with the result.

The second consumer is how QQ scales beyond one machine. It is also where the
public/private boundary is most likely to blur: a hosted product needs
tenancy, money, isolation, and orchestration, and none of that belongs in a
local-first harness. This document fixes the contract the supervisor relies
on, lists what the supervisor must own itself, and records the gaps a
supervisor currently works around so they can be closed in product-neutral
form.

## The Boundary

```text
supervisor (batch runner, CI, evaluation harness, hosted service)
  owns: tenancy, authorization, scheduling, leases, retries across attempts,
        isolation (container/VM/network), model gateway and spend authority,
        repository checkout, patch extraction, independent verification,
        artifact storage, billing, product identity
                          │
                          │  argv + environment + inline config    (in)
                          │  JSONL records on stdout, exit code    (out)
                          ▼
qq run  (one process, one workspace, one run)
  owns: agent loop, providers, tools, approvals, run limits, durable
        session store, structured events, typed outcome
```

Rule of thumb. If a behavior must work with **one machine, one user, and no
network other than the model endpoint**, it belongs in QQ. If it requires
**more than one tenant, more than one worker, or an authoritative record of
money**, it belongs to the supervisor. `architecture.md` already defers the
supervisor set (cloud control plane, distributed workers, multi-user tenancy);
this document makes the seam between the two explicit.

Consequences:

- QQ is consumed as a **binary** by a supervisor, never linked into the
  supervisor's process. A supervisor that executes untrusted repository code
  must not share a process with the code that runs it.
- `qq-core` is the backbone of autonomous work *inside the execution
  environment*. It is not the control plane, and it does not grow a control
  plane.
- Product-specific vocabulary (tenant, release, ledger, broker, approval
  queue, billing) does not appear in QQ. QQ speaks in gateways, correlation,
  run limits, outcomes, and events.
- Anything a supervisor needs from QQ is expressed as a generic feature that a
  local user, a CI job, and an evaluation harness can also use. There is no
  supervisor-only mode.
- The contract below is public. Golden fixtures for it live in this
  repository so a supervisor can pin and test against them without reading
  QQ source.

## The Contract Today

Verified against the working tree at the date above. Line references are
approximate anchors, not stable identifiers.

### Invocation

```sh
qq run [--workspace PATH] [--session ID]
       [--approval read-only|auto|full] [--profile NAME]
       [--allow-tool NAME]... [--allow-shell PREFIX]... [--steer-stdin]
       [--timeout-seconds N] [--max-turns N] [--max-cost-usd VALUE]
       [--correlation KEY=VALUE]... [--output-schema PATH] [--output-repair-turns N]
       [--format text|jsonl] [--trace PATH]
       [--model PROVIDER/MODEL] [--max-output-tokens N] [--organization NAME]
       -- PROMPT
```

Source: `src/cli.rs`. `--max-turns` is `u32` (widened from `u16` in protocol version 18). `ask` is not representable in
headless mode because there is no one to ask. `--max-cost-usd` requires
pricing for the selected model in configuration and exits `2` otherwise
(`src/main.rs`). `--correlation` labels are validated against the protocol's
bounds (8 entries, 64-byte keys, 256-byte values, 2 KiB total; a repeated
key is an error) before any configuration is read, stamped on both the
session and the run, and never interpreted.

`--output-schema PATH` imposes a typed-output contract (ADR-0014): the final
answer must be one JSON document satisfying the JSON Schema at `PATH`. The
file is read (bounded to the schema ceiling) and compiled before configuration
loads; an unreadable, oversized, malformed, referencing, or unsupported schema
exits `2` naming the path. `--output-repair-turns N` (0–8, default 2, requires
`--output-schema`) bounds the extra model turns the runtime may spend when an
answer fails validation. The model sees the schema in its system prompt from
the first turn. In text format a valid answer prints as the pretty-printed
validated document rather than the raw model text.

`--session ID` submits into an existing session instead of creating one. The
session must be a root session of the workspace (a spawned sub-agent session
is refused) and idle with no queued prompt; an unknown id and a session of
another workspace are the same `invalid_configuration` refusal, so the id's
existence elsewhere is not disclosed. The invocation decides the run exactly
as it would for a new session: the configured model (after `--model`),
`--profile`, and `--approval` are written to the session before the prompt is
submitted. Store ownership (below) and the recovery sweep both precede this,
so an earlier run the previous process left executing is already settled as
`interrupted` when the resume is examined, and its history — including the
runtime notice not to retry interrupted tool calls — is what the resumed run
sees. The event stream starts at the new prompt, not at the session's history
or the settings writes.

The id is made visible where a person can copy it. In text format, when stderr
is a terminal, `qq run` ends with a resume hint on stderr naming the exact
`qq run --session ID "<prompt>"` command, after any outcome (an interrupted or
exhausted run is exactly when someone wants to continue). The TUI prints the
same hint for the focused session after `/quit`, once the terminal is
restored. JSONL output never carries the hint: the id is in the `trial`
record, and a piped stderr receives nothing it did not ask for.

The interactive surface mirrors this: bare `qq` starts a new session, and
`qq --session ID` opens that session in the TUI. A session `qq run` left
behind can therefore be picked up interactively, and one the TUI left behind
can be driven headlessly. The TUI refuses an unknown id, one from another
workspace, or a spawned sub-agent session before painting anything, with the
same wording `qq run --session` uses.

### Configuration Injection

`QQ_CONFIG_CONTENT` carries one inline RON document (at most 1 MiB), applied
after project layers and before managed layers, bypassing the project trust
prompt (`crates/qq-config/src/loader.rs`). A supervisor uses it to point QQ at
a model gateway with an environment-backed credential reference:

```ron
(version: 1, model: "gateway/<model>",
 providers: {"gateway": LiteLlm(connection: (
     base_url: "http://<gateway>/v1", api: OpenAiChatCompletions,
     auth: ApiKey(Env("RUN_CREDENTIAL_VAR"))),
   models: {"<model>": (pricing: (input_usd_nanos_per_token: ..,
     output_usd_nanos_per_token: .., provenance: ".."))})})
```

`require_https` defaults to `false` and `allow_custom_providers` to `true`, so
a plain-HTTP gateway inside an isolated network is accepted. Secrets are
referenced, never inlined.

### State Location

`qq run` always creates a new session in `<data_dir>/sessions.sqlite3`, where
`data_dir` honors `XDG_DATA_HOME` on Linux (`crates/qq-config/src/loader.rs`).
A supervisor that wants the session store as an artifact redirects
`XDG_DATA_HOME` to a run-scoped directory.

One process owns a store at a time (ADR-0022). A second `qq` opening the same
store — a retry that starts before the previous attempt has exited, or two
runs sharing a data directory — exits `4` (`harness_failure`) with "session
store is owned by another running qq process" after a bounded wait, without
creating, opening, or recovering the database. It is safe to retry once the
owner exits; concurrent runs need distinct `XDG_DATA_HOME`s.

### Output: JSONL Records

With `--format jsonl`, stdout carries one JSON object per line, tagged by
`type`. The shapes are protocol vocabulary (`qq_protocol::HeadlessRecord`
with `HeadlessTrial`, `HeadlessOutcome`, `HeadlessStatus`; ADR-0023) and are
pinned byte-for-byte by the golden streams under
`crates/qq-protocol/tests/fixtures/headless/v<PROTOCOL_VERSION>/`, one per
exit status. Decoding a record with an unknown `type`, an unknown field, or an
unknown `status` fails; a supervisor should do the same.

| `type` | Fields | Notes |
| --- | --- | --- |
| `trial` | `qq_version`, `qq_source_revision`, `protocol_version`, `workspace_identity`, `model`, `profile`, `context_window?`, `pricing_provenance?`, `approval`, `timeout_seconds?`, `max_turns?`, `max_cost_usd_nanos?`, `correlation?`, `arm?`, `output_schema_sha256?`, `output_repair_turns?`, `workspace_id`, `session_id`, `run_id` | Exactly one, first, unless startup fails before a session exists. `correlation` is present only when at least one `--correlation` was given; the two `output_*` fields only with `--output-schema` (the hash is SHA-256 of the compact canonical schema encoding) |
| `event` | `envelope: SessionEventEnvelope` | `{ cursor: { sequence }, session_id, run_id?, caused_by?, occurred_at_ms, event: { type, ... } }`. Includes child-session events for the workspace |
| `outcome` | `status`, `exit_code`, `message?`, `usage?`, `estimated_cost_usd_nanos?`, `prompt_identity?`, `audit?`, `final_output?` | Exactly one, last. `final_output` is present only for a completed run submitted with `--output-schema`: `{ "status": "valid", "value": <json>, "repair_turns": n }` or `{ "status": "invalid", "errors": ["<pointer>: <message>", …], "repair_turns": n }` |

`--trace PATH` writes the same records to a file in either format.

### Exit Codes And Outcome Status

| `status` | exit | Meaning |
| --- | ---: | --- |
| `completed` | 0 | Run finished; the model produced a final answer |
| `task_failed` | 1 | Run finished with a failure the agent reported, a non-server run failure, or a completed answer that never satisfied `--output-schema` (`final_output.status == "invalid"`) |
| `invalid_configuration` | 2 | QQ refused to start: config, model, pricing, or flag error |
| `timed_out` | 3 | `max_duration_ms` limit reached |
| `budget_exhausted` | 3 | Any other run limit reached (turns, cost, tokens, tool calls) |
| `harness_failure` | 4 | QQ itself failed (store, provider protocol, internal) |
| `interrupted` | 130 | Signal or cancellation |

Exit `3` is shared by two statuses. A supervisor must read `outcome.status`
and should require `outcome.exit_code == <process exit>` before trusting the
status. A process exit without a matching `outcome` record (for example an
external `SIGKILL`, exit 137) is a harness or infrastructure failure, never a
timeout.

`estimated_cost_usd_nanos` is QQ's estimate from configured pricing and
observed usage. It is evidence, not an authoritative charge; a supervisor with
a gateway that meters spend treats the gateway's record as authoritative.

### Approval Semantics In Headless Mode

Approval classification uses the catalog effect class (`crates/qq-core/src/approval.rs`):

- `read-only`: read-only tools execute; every mutating, shell, or external
  call is **denied** before a request exists. `--allow-*` never fires.
- `auto`: everything executes except shell commands the policy classifies as
  dangerous (`sudo`, recursive `rm`, `git push`, `curl | sh`, and similar);
  those are held and answered by `--allow-tool`/`--allow-shell` grants.
- `full`: everything executes; grants are redundant.

`--allow-tool` and `--allow-shell` therefore **widen** what a held call may
do; they are not an allowlist that narrows the catalog. Configuration
`policy.allow_tools` and `policy.allow_shell_prefixes` also declare grants.
Managed `policy.deny_tools` and `policy.deny_shell_prefixes` filter those
grants; they do not remove tools from the catalog or prohibit an otherwise
approved call (`crates/qq-config/src/document.rs`, `resolve_policy_grants`).
Choose `qq run --profile <name>` to select a pre-compiled profile/pack catalog.
Optional `policy.exposed_tools` narrows that catalog further: absent means no
additional restriction, `[]` exposes no tools, and lists intersect across
configuration layers. For example, `exposed_tools: ["read_file", "search"]`
exposes only those tools even under `--approval full`. A grant cannot restore
a hidden tool. Restricting exposure requires no workspace trust; grants in
the same document still require their ordinary trust approval.

`config check` validates exact built-in names and MCP name syntax without
discovering tools. Compilation checks MCP membership among the servers
admitted by the profile. Configured servers excluded by a profile's MCP
subset remain excluded without discovery; an unknown server or a missing
tool on an admitted server fails compilation. `load_skill` is a known name
but appears only when workspace or pack skills are available. Large external
catalogs stay progressive when `select_tools` is exposed; omitting the
selector sends the permitted schemas directly, within the existing catalog
bounds. A discovered name still passes ordinary schema and catalog admission;
an oversized tool is excluded with its existing typed reason while valid
peers remain available.

Built-in file tools are
contained to the workspace through capability-scoped file handles; the `shell`
tool is **not** contained. Isolation is the supervisor's job.

### Run Limits

`RunLimits` (`crates/qq-protocol/src/sessions.rs`) supports `max_duration_ms`,
`max_model_turns`, `max_tool_calls`, `max_total_tokens`, `max_cost_usd_nanos`,
`max_input_tokens`, `max_output_tokens`, `max_tool_output_bytes`,
`max_children`, `max_concurrent_children`. The core budget meter enforces the
duration, turn, tool-call, token, cost, and output bounds with typed
`budget_exhausted` outcomes. Child admission enforces the other two limits:
exceeding `max_children` returns a tool error that lets the parent continue,
and `max_concurrent_children` limits concurrent admission through a semaphore
(`crates/qq-core/src/sessions/subagents.rs`). `qq run` exposes only duration,
turns, and cost on the command line. Cost and token caps settle as
`cost_unknown` / `tokens_unknown` when the provider does not report usage; a
supervisor must not assume a limit held when the signal was absent.

### Plan Identity

Every accepted run records a secret-free `AgentPlanDescriptor` and its digest
(`crates/qq-core/src/plan/descriptor.rs`). The descriptor includes the
canonical workspace path, the provider endpoint, and configuration source
provenance. The digest therefore changes across checkout paths and gateway
URLs. It identifies *this execution's plan*, not *this configuration*. A
supervisor that needs configuration-only identity computes it from its own
release artifact; it must not strip fields from QQ's digest and call the
result stable.

### `qq serve`

The HTTP/SSE server (`crates/qq-server`) binds loopback only, generates a
per-instance bearer token, and is single-instance per host. It is the right
surface for an interactive client or a co-located supervisor on the same
machine. It is **not** a hosted API: no tenancy, no per-caller authorization,
no non-loopback bind without an explicit application authentication design.
See `protocol.md`.

## What The Supervisor Owns

These are not gaps. They are deliberately outside QQ and should stay there.

| Concern | Why it is not QQ's |
| --- | --- |
| Isolation (container, VM, network policy) | QQ contains file tools, not processes. A supervisor never trusts approval policy as a sandbox |
| Repository checkout and credentials | QQ operates on a directory it is given; the credential used to fetch it must never enter the run |
| Patch extraction (`git diff` from a base commit) | QQ edits in place and records per-call diffs; aggregate patch semantics are a workflow decision |
| Spend authority | QQ estimates; a metering gateway reserves and settles |
| Attempts, leases, fencing, restart | QQ has one run per process; retry across processes is orchestration |
| Independent verification | Running checks in a fresh environment the agent never touched is the supervisor's evidence, not QQ's |
| Artifact storage, retention, tenancy, billing, identity | Product concerns |

## Gaps A Supervisor Currently Works Around

Each of these is a generic QQ improvement that local, CI, and evaluation
users also benefit from. They are tracked as the headless-contract tranche
(HC1–HC4) in
[`../plans/speed-first-extensible-agent-harness.md`](../plans/speed-first-extensible-agent-harness.md).
CLI parsing and schema compilation stay off the run hot path. HC1 also
changes shared limit types and startup ownership; HC3 adds opt-in core
validation and repair turns, with bounded work and measured performance.

| Gap | Today | Intended | Task |
| --- | --- | --- | --- |
| Correlation from the CLI | **Shipped** 2026-09-11 (`f0b7dd3`). `--correlation KEY=VALUE` (repeatable) is validated as one set against the protocol bounds before configuration loads, stamped on the session and the run, and echoed on `trial` (omitted when empty) and every session snapshot | — | HC1 |
| Resume into an existing session | **Shipped** 2026-09-11 (`63cb256`, `63032ab`; ADR-0022). Every store open takes an advisory owner lock before SQLite is opened and before recovery; a busy store is refused as `StoreBusy` without database I/O. `qq run --session ID` then submits into an idle root session of the workspace, applying the invocation's model, profile, and approval first; an interrupted earlier run is already settled by recovery and is never re-executed | — | HC1 |
| `--max-turns` width | **Shipped** 2026-09-11 (`d079e21`). `RunLimits.max_model_turns`, every `turn_ordinal`, the core turn loop, the budget meter, and the CLI are `u32`; `PROTOCOL_VERSION` 17 → 18 with `v18/` goldens and `v17/` retained decode-only; boundary tests pin 65 536 and `u32::MAX` on the wire and drive the meter past 65 535 without a provider | — | HC1 |
| Minimal configuration check | **Shipped** 2026-09-11 (`95c6e3d`). `ConfigLoader::check` validates every rule and treats only `ModelRequired` as "valid apart from the selection"; `config check` with `(version: 1)` passes and names the missing model. `load()` and every run-time path still require one | — | HC1 |
| Narrowing tool exposure | **Shipped** 2026-09-06 (`93ef6b8`). Optional `policy.exposed_tools` narrows the catalog by intersection across layers and existing profile/pack exposure. An absent field adds no restriction; an empty list exposes no tools. Existing grants and managed grant denies retain their meaning. Static names and MCP name syntax validate during `config check`; profile-admitted MCP membership validates during plan compilation without discovery in `config check`; ordinary catalog bounds remain authoritative | — | HC2 |
| Typed final output | **Shipped** 2026-09-12 (`feat/hc3-typed-final-output`; ADR-0014). `--output-schema PATH` and `--output-repair-turns N` compile a bounded, reference-free JSON Schema subset before configuration loads; the contract rides `submit_prompt.output`, is persisted on the run row and re-enforced after restart; core validates the answer that survived audit and steering, repairs within the allowance, and settles `Completed` with `final_output` (`valid` with the parsed value, or `invalid` with bounded `<pointer>: <message>` errors) written in the settlement transaction and published on `run_finished` and `outcome`. `PROTOCOL_VERSION` 18 → 19, store schema 26 → 27. Valid JSON is not a correct answer; the supervisor still verifies | — | HC3 |
| Pinning the contract | **Shipped** 2026-09-13 (`feat/hc4-headless-goldens`; ADR-0023). The record shapes moved into `qq-protocol` as `HeadlessRecord`/`HeadlessTrial`/`HeadlessOutcome`/`HeadlessStatus`; the binary emits through a borrowing view whose encoding a test pins to the owned type. `crates/qq-protocol/tests/fixtures/headless/v19/` holds ten complete streams (every exit status, the default payload, every optional trial field, both `final_output` verdicts) checked byte-for-byte and for framing; `v18/` holds the default-path streams decode-only. The binary's own tests decode every stdout line strictly and require it to re-encode identically | — | HC4 |
| Exit code `3` ambiguity | Shared by `timed_out` and `budget_exhausted` | Keep the codes; the status field is authoritative and the fixtures pin that. Splitting the code is a breaking change with no consumer asking for it. Revisit only with a real request | none |
| Static binary | musl build fails in Cargo build scripts | Packaging, not contract. Tracked outside this document | none |

HC3 accepts at most 64 KiB of schema JSON, 32 nesting levels, and 4096 JSON
values, including enum values. It rejects unsupported keywords and all
references before session creation; compilation and validation perform no
network discovery or external-reference fetches. The supported keywords are
`type`, `enum`, `const`, `properties`, `required`, `additionalProperties`,
`items`, `minItems`/`maxItems`/`uniqueItems`, `minLength`/`maxLength`,
`minimum`/`maximum`/`exclusiveMinimum`/`exclusiveMaximum`,
`minProperties`/`maxProperties`, `anyOf`/`oneOf`/`allOf`/`not`, and the
annotations `$schema`, `$id`, `$comment`, `title`, `description`, `default`,
`examples`. `--output-repair-turns` is bounded to 0–8 (default 2) for the
entire run. Each validation-error payload, including its rendered feedback or
durable result, is at most 8 KiB (at most 16 errors); the repair bound limits
repeated feedback. Repairs consume ordinary run budgets (a repair that becomes
the reserved budget-final turn settles as `budget_exhausted` with no verdict),
support cancellation and steering, and do not reset their allowance after an
audit revision or steering. The final answer, including any bounded audit
revision, is what the contract judges. HC3 retains the audit's existing
revision bound. A single ```` ```json ```` fence around the document is
tolerated. Measured on the recording host: compile ≈37 µs and validate ≈11 µs
for a 64-property schema and a 7 KiB answer; the schema-less default path is
unchanged (`read_tool_loop` median 54.8 → 52.2 µs, within noise).

## Compatibility Policy

- `PROTOCOL_VERSION` (`crates/qq-protocol/src/lib.rs`) governs the envelope
  and event vocabulary. The JSONL record shapes above are part of that
  contract and bump with it (ADR-0023); their golden streams live under
  `crates/qq-protocol/tests/fixtures/headless/v<PROTOCOL_VERSION>/` and
  every retained earlier directory must still decode. Version 19 added the
  optional `submit_prompt.output`, `run_finished.final_output`, and the
  trial/outcome fields above; every default-path version-18 stream is
  byte-identical after the version field changes (the `v18/` and `v19/`
  default-path goldens differ only there), and `capabilities.limits` gained
  four declared bounds.
- New fields are additive and optional and are omitted, never `null`, when
  absent. A supervisor may ignore unknown fields and must fail closed on
  unknown `type` or `status` values; `qq_protocol::HeadlessRecord` itself
  rejects both.
- Widening the shared model-turn limit to `u32` changes the accepted wire
  range and requires a protocol-version bump. Existing `u16`-range records
  remain decodable; tests cover values above that range without executing
  tens of thousands of turns.
- Each fixture version pins its exact `protocol_version`. Cross-version
  default-path comparisons permit declared version-field changes after
  normalizing run identity, timestamps, and build metadata; they require
  unchanged application payload and no new opt-in fields without flags. The
  golden streams are constructed, not recorded: they pin shapes and framing
  (one leading `trial` unless startup failed, only `event`s between, strictly
  increasing cursors, one trailing `outcome` whose `exit_code` agrees with
  its `status`), not a transcript.
- Exit codes are stable. A new terminal status reuses an existing code and is
  distinguished by `status`.
- `estimated_cost_usd_nanos`, `usage`, and `prompt_identity` are diagnostic.
  Their absence is not an error.
- `AgentPlanDigest` is not a configuration identity and may change without a
  protocol bump when the descriptor version changes.

## Review Questions For Future Additions

Before adding a headless flag, record, or field, answer:

1. Would a local user, a CI job, and an evaluation harness all plausibly use
   it? If only a hosted product would, it belongs in the supervisor.
2. Does it introduce product vocabulary? Rename it in QQ's terms or leave it
   out.
3. Does it require QQ to hold authority it should not (money, tenancy,
   isolation)? Leave it out.
4. Is it additive to the JSONL contract, or does it change an existing
   field's meaning? The latter needs a `PROTOCOL_VERSION` bump and new
   fixtures.
5. Does it touch the run hot path? Keep CLI plumbing and compilation off it.
   An opt-in runtime feature such as typed-output validation needs explicit
   bounds, cancellation and budget tests, and measurements of both its
   enabled cost and the unchanged default path.
