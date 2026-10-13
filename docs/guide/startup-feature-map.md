# Feature map

Use this table to move from a user journey to its source entrypoint and guide.
The status is for the reviewed snapshot in the [overview](startup-overview.md).

## QQ features

| Feature or process | Source entrypoint | Guide | Status |
| --- | --- | --- | --- |
| CLI dispatch | [`src/cli.rs`](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/cli.rs) and [`src/main.rs`](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/main.rs) | [CLI](cli.md) | Implemented source |
| TUI sessions and rendering | [`crates/qq-tui/`](https://github.com/retsu-AI/qq/tree/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-tui) | [The TUI](tui.md) | Implemented source |
| Configuration layers | [`crates/qq-config/`](https://github.com/retsu-AI/qq/tree/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-config) | [Configuration](configuration.md) | Implemented source |
| Trust and project policy | [`docs/guide/permissions.md`](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/docs/guide/permissions.md) | [Permissions](permissions.md) | Implemented source |
| Provider adapters | [`crates/qq-provider/`](https://github.com/retsu-AI/qq/tree/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-provider) | [Providers](providers.md) | Implemented source |
| Credential storage and OAuth | [`crates/qq-auth/`](https://github.com/retsu-AI/qq/tree/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-auth) | [Providers](providers.md) | Implemented source |
| Sessions, history, replay, and persistence | [`crates/qq-core/`](https://github.com/retsu-AI/qq/tree/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-core) | [Concepts](concepts.md) | Implemented source |
| Headless `ask`, `run`, and `serve` | [`src/headless.rs`](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/headless.rs) and [`crates/qq-server/`](https://github.com/retsu-AI/qq/tree/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-server) | [Headless](headless.md) | Implemented source |
| Built-in tool permissions | [`docs/design/tools.md`](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/docs/design/tools.md) | [Permissions](permissions.md) | Implemented source |
| Packs, profiles, and skills | [`crates/qq-config/`](https://github.com/retsu-AI/qq/tree/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-config) | [Configuration](configuration.md) | Implemented source |
| MCP client and catalog | [`crates/qq-mcp/`](https://github.com/retsu-AI/qq/tree/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-mcp) | [MCP](mcp.md) | Implemented source |
| JEV observer, routing, and approval | [`src/advisory.rs`](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/advisory.rs), [`src/cli.rs`](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/cli.rs) | [CLI](cli.md) | Implemented source |

## Startup features

Retsu rows use origin/main pin `8d58080829231bb74e180c6de4e8f687c4606200`, refreshed on 2026-10-13. Private repository access is required. The older `b889cd9e531438e4c54b25aa5ff9dc281c5e6888` snapshot is historical.

| Feature or process | Reviewed source | Guide | Status |
| --- | --- | --- | --- |
| Assistant project and memory journal | Rust assistant `docs/units/U1A.md`, `U2A.md`, `U2B-CLI.md` | [Assistant](startup-assistant.md) | Candidate source |
| Assistant gateway and nodes | Rust assistant plus Mac and iOS private worktrees | [Assistant](startup-assistant.md) | Candidate source |
| Direct and assistant native modes | `SPEC.md`, `SHARED-CLIENT-CONTRACT.md` in Mac and iOS candidates | [Native modes](startup-native-modes.md) | Candidate source |
| Retsu Effect gateway | [`packages/api/src/index.ts`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/index.ts), [`http/public.ts`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/http/public.ts) | [API and billing](startup-api-billing.md) | Implemented source |
| Retsu auth and sessions | [`services/Identity.ts`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Identity.ts), [`services/Sessions.ts`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Sessions.ts), [`http/account.ts`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/http/account.ts) | [API and billing](startup-api-billing.md) | Implemented source |
| Retsu extraction and usage | [`services/Extract.ts`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Extract.ts), [`services/Ledger.ts`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Ledger.ts) | [API and billing](startup-api-billing.md) | Implemented source |
| Stripe checkout, refunds, and disputes | [`services/Billing.ts`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/services/Billing.ts), [`http/account.ts`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/http/account.ts) | [API and billing](startup-api-billing.md) | Implemented source |
| Grip model and SDK | `crates/grip-server/`, `docs/reference/api.md`, SDK directories | [Data and models](startup-data-models.md) | Planned |
| Platform2 dataset and annotation loop | `docs/09-local-reviewed-data-experiment.md`, `docs/LOCAL-MODEL-LOOP-FEATURE-MAP.md` | [Data and models](startup-data-models.md) | Candidate source |
| Cloudflare gateway and web | [`packages/api/wrangler.jsonc`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/wrangler.jsonc), [`packages/web/worker/index.ts`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/web/worker/index.ts) | [Infrastructure](startup-infrastructure.md) | Implemented source |
| Modal Grip backend | [`packages/api/worker-configuration.d.ts`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/worker-configuration.d.ts), `infra/modal/` | [Infrastructure](startup-infrastructure.md) | Implemented source |
| AWS ECS and Temporal target | Selected startup specification | [Infrastructure](startup-infrastructure.md) | Planned, not deployed |
| Tracing, request IDs, health, catalog | `crates/qq-server/src/lib.rs`, Retsu [`http/respond.ts`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/http/respond.ts), gateway [`index.ts`](https://github.com/retsu-AI/retsu/blob/8d58080829231bb74e180c6de4e8f687c4606200/packages/api/src/index.ts) | [Observability](startup-observability.md) | Implemented source |
| Sentry error routing | Selected startup specification | [Observability](startup-observability.md) | Planned |

Private repositories and candidate branches require access. A table row does
not certify its runtime, deployment, provider, device, billing, or customer
acceptance.
