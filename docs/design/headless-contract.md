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
qq run [--workspace PATH] [--approval read-only|auto|full] [--profile NAME]
       [--allow-tool NAME]... [--allow-shell PREFIX]... [--steer-stdin]
       [--timeout-seconds N] [--max-turns N] [--max-cost-usd VALUE]
       [--format text|jsonl] [--trace PATH]
       [--model PROVIDER/MODEL] [--max-output-tokens N] [--organization NAME]
       -- PROMPT
```

Source: `src/cli.rs`. `--max-turns` is `u16`. `ask` is not representable in
headless mode because there is no one to ask. `--max-cost-usd` requires
pricing for the selected model in configuration and exits `2` otherwise
(`src/main.rs`).

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

### Output: JSONL Records

With `--format jsonl`, stdout carries one JSON object per line, tagged by
`type` (`src/headless.rs`, `TrialRecord`):

| `type` | Fields | Notes |
| --- | --- | --- |
| `trial` | `qq_version`, `qq_source_revision`, `protocol_version`, `workspace_identity`, `model`, `profile`, `context_window?`, `pricing_provenance?`, `approval`, `timeout_seconds?`, `max_turns?`, `max_cost_usd_nanos?`, `arm?`, `workspace_id`, `session_id`, `run_id` | Exactly one, first, unless startup fails before a session exists |
| `event` | `envelope: SessionEventEnvelope` | `{ cursor: { sequence }, session_id, run_id?, caused_by?, occurred_at_ms, event: { type, ... } }`. Includes child-session events for the workspace |
| `outcome` | `status`, `exit_code`, `message?`, `usage?`, `estimated_cost_usd_nanos?`, `prompt_identity?`, `audit?` | Exactly one, last |

`--trace PATH` writes the same records to a file in either format.

### Exit Codes And Outcome Status

| `status` | exit | Meaning |
| --- | ---: | --- |
| `completed` | 0 | Run finished; the model produced a final answer |
| `task_failed` | 1 | Run finished with a failure the agent reported or a non-server run failure |
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
| Correlation from the CLI | `qq run` passes `Correlation::default()`; the protocol already supports up to 8 bounded entries and returns them in snapshots | `--correlation KEY=VALUE` (repeatable) stamped on the session and echoed in `trial` and every envelope's session snapshot | HC1 |
| Resume into an existing session | `qq run` always creates a session; `SubmitPrompt` on an existing session exists only through the server | `qq run --session ID` establishes exclusive store ownership before opening the runtime or running recovery, rejects a busy store without mutation, then submits into an idle session in the same workspace; recovery never replays uncertain side effects | HC1 |
| `--max-turns` width | CLI, `RunLimits.max_model_turns`, and the core turn counter are `u16` | Widen the shared types and CLI to `u32`, with a protocol-version bump, historical decoding fixtures, and boundary tests | HC1 |
| Minimal configuration check | `config check` with `(version: 1)` fails with "model must be configured" | A document with no model validates; model selection is checked at run time, where it already is | HC1 |
| Narrowing tool exposure | Implemented in `93ef6b8`; local behavior and failure tests pass, combined integration/H0 qualification pending | Optional `policy.exposed_tools` narrows the catalog by intersection across layers and existing profile/pack exposure. An absent field adds no restriction; an empty list exposes no tools. Existing grants and managed grant denies retain their meaning. Validate static names and MCP name syntax during `config check`, then profile-admitted MCP membership during plan compilation without discovery in `config check`; ordinary catalog bounds remain authoritative | HC2 |
| Typed final output | The final answer is free text; a supervisor that wants a structured report parses prose or asks the model to write a file | Compile `--output-schema PATH` off the run hot path; core validates the final answer and performs bounded repair turns. Optional `final_output` on `RunFinished` and `outcome` carries the validated value or a typed validation failure, persisted before publication. Valid JSON is not a correct answer; the supervisor still verifies | HC3 |
| Pinning the contract | Supervisors re-read QQ source at each bump | Golden JSONL fixtures for `trial`/`event`/`outcome` per `PROTOCOL_VERSION` under `crates/qq-protocol/tests/fixtures/headless/`, with a compatibility statement in this document | HC4 |
| Exit code `3` ambiguity | Shared by `timed_out` and `budget_exhausted` | Keep the codes; the status field is authoritative and the fixtures pin that. Splitting the code is a breaking change with no consumer asking for it. Revisit only with a real request | none |
| Static binary | musl build fails in Cargo build scripts | Packaging, not contract. Tracked outside this document | none |

HC3 accepts at most 64 KiB of schema JSON, 32 nesting levels, and 4096 JSON
values, including enum values. It rejects unsupported keywords and all
references before session creation; compilation and validation perform no
network discovery or external-reference fetches. `--output-repair-turns` is
bounded to 0–8 (default 2) for the entire run. Each validation-error payload,
including its rendered feedback or durable result, is at most 8 KiB; the
repair bound limits repeated feedback. Repairs consume ordinary
run budgets, support cancellation and steering, and do not reset their
allowance after an audit revision or steering. The final answer, including
any bounded audit revision, must pass validation before a valid result can
be published. HC3 retains the audit's existing revision bound. Its acceptance
tests cover these interactions and replay; enabled validation and repair
cost is measured separately from the schema-less default path.

## Compatibility Policy

- `PROTOCOL_VERSION` (`crates/qq-protocol/src/lib.rs`) governs the envelope
  and event vocabulary. The JSONL record shapes above are part of that
  contract from HC4 onward and bump with it.
- New fields are additive and optional; a supervisor must ignore unknown
  fields and must fail closed on unknown `type` or `status` values.
- Widening the shared model-turn limit to `u32` changes the accepted wire
  range and requires a protocol-version bump. Existing `u16`-range records
  remain decodable; tests cover values above that range without executing
  tens of thousands of turns.
- Each fixture version pins its exact `protocol_version`. Cross-version
  default-path comparisons permit declared version-field changes after
  normalizing run identity, timestamps, and build metadata; they require
  unchanged application payload and no new opt-in fields without flags.
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
