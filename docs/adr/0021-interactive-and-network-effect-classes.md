# ADR-0021 — `Interactive` and `Network` effect classes: a question is a hold, not a permission; a fetch is authority over the outside, not the workspace

**Status:** Accepted (`Interactive` merged in #49, `Network` in #50)
**Date:** 2026-09-15
**Deciders:** tool-layer plan T8/T9
**Implements:** [`tool-layer.md` § D7](../plans/tool-layer.md#d7--fetch-ask_user-view_image-select_tools-t8-t9-t11),
[`tool-layer.md` § New effect classes](../plans/tool-layer.md#new-effect-classes-and-wire-impact),
[`tools.md` § Approval Policy](../design/tools.md#approval-policy),
[`tools.md` § Asking The User](../design/tools.md#asking-the-user)

## Context

Every tool call carries one `EffectClass` from the catalog, and policy
decides `Execute | RequireApproval | Deny | Forbidden` from that class and
the session's approval mode. Until T8 there were four classes — `ReadOnly`,
`Mutating`, `Shell`, `External` — and every non-read call was a request for
permission to act on the workspace or through a host. Two planned tools do
not fit that shape.

`ask_user` acts on nothing. The model wants a decision from the human and a
wrong guess costs a retry loop of tool calls and tokens; ending the turn with
a question in prose is worse still, because in `qq run` the run then exits
"completed" with a question as its answer and nothing upstream can tell.
Classifying it `Mutating` would deny it under `read-only` (where consulting
the user is the safest possible move) and hold it for a reviewer under
`supervised` (where there is nothing to adjudicate). Classifying it
`ReadOnly` would execute it — but there is nothing to execute; the human's
reply is the result.

`fetch` acts on the outside world. Under today's classes it would be
`External` (gated like an MCP tool) or `Shell` (`curl` through the
classifier). Neither expresses what the human is actually approving: that
this run may reach a particular host. Host-shaped grants, an SSRF deny list
that no mode may override, and a managed `deny_hosts` need their own row in
the decision table.

Both tools are additive protocol changes: the reused approval wait gains a
`question`, the decision gains `answer`, the resolution gains `answered`, and
the grant gains `host` with a `fetch` preview. Protocol 21 carries the first
three (with T12's `range`); protocol 22 carries the grant and preview.

## Decision

`EffectClass` gains two variants, stored as `"interactive"` and `"network"`
in the existing text column (no migration).

**`Interactive` holds under every mode and settles from the answer.** Policy
returns `AskUser { question }` for a well-formed `ask_user` call regardless of
mode or grants; the session gate registers the waiter, persists the call as
`awaiting_approval`, and publishes `tool_approval_requested` with the parsed
`question` and no `shell`/`edit` preview — the same registration-before-
publish order as an approval, so an answer can never race past the waiting
run. The reviewer is not consulted. A client answers with
`ApprovalDecision::Answer { answers }`; in one transaction the store settles
the call `completed`, `is_error = 0`, with the rendered questions and
answers as `result`, and resolves the hold `answered`. The gate returns a new
`GateDecision::Answered { result }` and the run loop records the result
without dispatching. An empty answer set declines with a fixed result that
tells the model to proceed. The approval timeout applies unchanged
(`denied_timeout`). Malformed arguments carry no question, evaluate to
`Execute`, and reach dispatch only to return the contract error.

Headless has no answerer. `qq run` cancels at the first question and exits
`needs_input` (5), naming the question in `outcome.message`; a child
session's question is declined immediately. Direct runs without a session
gate deny with a fixed "no user is available" result. The prompt tells the
model to ask once, offer concrete options, and never ask what a tool could
find out.

**`Network` is denied under `read-only`, asks under `ask` and `supervised`,
executes under `auto` only when a grant covers the host, and executes under
`full`** — but the SSRF set (loopback, RFC 1918, shared, link-local, ULA,
IPv4-mapped and NAT64 forms, `.local`/`.internal`/single-label names, cloud
metadata) and managed `deny_hosts` are refused under every mode, exactly
like a shell `Forbidden`. The name is judged at classification, before the
gate; the resolved addresses are judged at dispatch and the client is pinned
to them (`resolve_to_addrs`), so a second lookup cannot rebind; every
redirect repeats both. Grants gain `Host { host }` (exact or one leading
`*.` wildcard, never the apex); `PolicyDecision::Deny` gains a `DenyReason`
so the model learns whether the mode or a host rule refused it. `auto` does
not execute an ungranted public host: the plan's "public or allowed" collapsed
to "allowed" because a public/private judgement by name alone is exactly the
rebinding gap, and one approval per site is cheap.

`ReadOnly` still runs concurrently; `Interactive`, like every other class,
is sequential in request order so a question is asked at a boundary the
model reasoned about.

## Consequences

- The decision table (`tools.md` § Approval Policy) has six rows; exhaustive
  matches on `EffectClass`, `ToolClass`, `PolicyDecision`,
  `ApprovalDecision`, and `ApprovalResolution` fail to compile until every
  consumer chooses, which is how the client, headless, and TUI arms were
  found.
- `ask_user` reuses the approval wait wholesale: one registry, one timeout,
  one persistence path, one TUI inline block. The TUI renders a question
  block with numbered options, digits pick, the composer takes free text,
  Esc declines; `y/a/w/n` are not approval keys while a question is pending.
- `HeadlessStatus` gains `needs_input` (5). Supervisors that switch on exit
  code see a new value; the exit table is pinned by a golden test.
- The `ask_user` schema is one more declaration in every request (schema
  hash pinned). Its cost is offset the first time it prevents a wrong-guess
  loop; T13 measures whether models over-ask.
- `fetch` adds `reqwest`, `url`, `ipnet`, and `htmd` (html5ever) to
  `qq-core`; `reqwest` was already in the binary through `qq-provider`, and
  the minimal `qq-provider` profile is unaffected. The `html2text`/`htmd`
  bake-off on documentation and navigation-heavy fixtures chose `htmd` for
  fenced code with language tags, pipe tables, inline links, and roughly half
  the output bytes.
- Fixture servers for tests sit on loopback, which the policy refuses; a
  test-only `allow_private_for_tests` flag on `NetworkPolicy` admits
  loopback and RFC 1918 (never metadata) and is unreachable from
  configuration.

## Alternatives considered

- **A tool that executes and blocks inside dispatch.** Would need a second
  wait registry, its own persistence for the pending state, and a way for
  the client command to reach a running tool. The approval path already does
  all of this durably.
- **`ask_user` as `ReadOnly`.** Executes immediately with nothing to
  execute; the question would have to be smuggled through a tool error or a
  side channel.
- **Answering headless questions with a default.** Hides the ambiguity the
  model surfaced; a supervisor cannot distinguish "chose A" from "guessed A".
  Exiting with a typed status is honest and resumable.
- **A `todo`/`plan` tool alongside `ask_user`.** Rejected in the plan: costs a
  call plus the list's tokens each update; the headless contract already
  surfaces progress through events.
