# Observability

Observability must help an operator answer which revision handled a request,
what state it reached, and whether an external side effect is still uncertain.
Keep credentials, raw prompts, memory content, and customer records outside
diagnostic events.

## QQ source signals

The QQ server exposes health, capabilities, model catalog, workspace snapshot,
and SSE workspace events. Its prompt, steer, approval, cancellation, and
session-profile command routes are defined by the
[COMMAND_ROUTES table](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-protocol/src/sessions.rs)
and served by
[crates/qq-server/src/lib.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-server/src/lib.rs).
The CLI and headless records define command and session boundaries in
[src/cli.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/cli.rs)
and [src/headless.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/headless.rs).

A useful incident record includes source or binary revision, command or route,
session/workspace scope, request or run identifier, event cursor, deadline,
expected result, observed result, and whether the provider outcome is known.
Health or a closed SSE stream proves transport state only; it does not prove
that a provider accepted a prompt.

## Current Retsu request evidence

The current private Retsu source pin is
`8d58080829231bb74e180c6de4e8f687c4606200` (commit dated 2026-10-09, origin/main fetched 2026-10-13;
repository access required). The earlier `b889cd9e531438e4c54b25aa5ff9dc281c5e6888`
snapshot is historical context. Current request identity and response
instrumentation are in
[packages/api/src/index.ts](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/index.ts)
and [packages/api/src/http/respond.ts](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/http/respond.ts).
[services/Extract.ts](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Extract.ts)
records typed extraction outcomes.

Current monitoring source is separate from proposed Sentry work:
[services/Metrics.ts](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Metrics.ts)
writes bounded request and event points to Analytics Engine, using bounded
route, method, outcome, status, latency, token, cost, cold-start, invariant,
hashed account-id (`blob4`), and key-mode (`blob7`) fields. [services/Monitor.ts](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Monitor.ts)
runs five-minute checks and daily digest crons for backend failures, 5xx rate,
ledger drift, webhook/mail failures, cold starts, and quota signals, then
sends configured chat/email alerts. These are source capabilities, not proof
that the current hosted bindings, crons, alerts, or dashboards are enabled.

## Planned error routing

Privacy-safe Sentry collection and deduplicated error-to-agent assignment are
proposed startup work. They are not enabled by the reviewed source and should
not be presented as a production collector. Before activation, specify event
schema, redaction, retention, access boundary, sampling, ownership routing,
and deletion path. A future error assignment must link to a bounded revision
and reproducible evidence, not expose a bearer token, prompt body, receipt,
account identifier, or private dump.

Use [Recovery](startup-recovery.md) to record uncertain effects and the repair
decision.
