# Observability

Observability must help an operator answer which revision handled a request,
what state it reached, and whether an external side effect is still uncertain.
Keep credentials, raw prompts, memory content, and customer records outside
diagnostic events.

## QQ source signals

The QQ server defines health, capabilities, model catalog, workspace snapshot,
SSE workspace events, prompt submission, steering, approvals, cancellation, and
session profile routes in
[crates/qq-server/src/lib.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-server/src/lib.rs).
The CLI and headless records define the command and session boundary in
[src/cli.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/cli.rs)
and [src/headless.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/headless.rs).

A useful incident record includes source or binary revision, command or route,
session/workspace scope, request or run identifier, event cursor, deadline,
expected result, observed result, and whether the provider outcome is known.
Health or a closed SSE stream proves transport state only; it does not prove
that a provider accepted a prompt.

## Retsu request evidence

The private Retsu snapshot at
`b889cd9e531438e4c54b25aa5ff9dc281c5e6888` (repository access required)
adds request identity at the gateway and records typed extraction outcomes:
[packages/api/src/index.ts](https://github.com/retsu-AI/retsu/blob/b889cd9e531438e4c54b25aa5ff9dc281c5e6888/packages/api/src/index.ts),
[packages/api/src/http/respond.ts](https://github.com/retsu-AI/retsu/blob/b889cd9e531438e4c54b25aa5ff9dc281c5e6888/packages/api/src/http/respond.ts),
and [packages/api/src/services/Extract.ts](https://github.com/retsu-AI/retsu/blob/b889cd9e531438e4c54b25aa5ff9dc281c5e6888/packages/api/src/services/Extract.ts).
The source names request IDs, spans, status, model, token counts, fields,
cost, and latency as bounded request/usage evidence. Account and billing
records remain separately governed; see [API and billing](startup-api-billing.md).

## Planned error routing

Privacy-safe Sentry collection and deduplicated error-to-agent assignment are
proposed startup work. They are not enabled by the reviewed source and should
not be presented as a production collector. Before activation, specify the
event schema, redaction, retention, access boundary, sampling, ownership
routing, and deletion path. A future error assignment must link to a bounded
revision and reproducible evidence, not expose a bearer token, prompt body,
receipt, account identifier, or private dump.

Use [recovery](startup-recovery.md) to record uncertain effects and the
repair decision.
