# Feature map

This map connects a user journey to its owner, source entrypoint, storage
boundary, guide, and evidence limit. A source pin identifies a revision. It
does not establish acceptance.

## QQ journeys

| Journey | Source entrypoint | Durable owner | Guide | Current basis |
| --- | --- | --- | --- | --- |
| Interactive terminal | `src/main.rs`, `crates/qq-tui/` | QQ session store | [The TUI](tui.md) | Implemented source |
| One-shot prompt | `src/main.rs::ask` | Provider response stream | [Headless](headless.md) | Implemented source |
| Durable headless run | `src/headless.rs`, `crates/qq-protocol/src/headless.rs` | QQ SQLite store | [Headless](headless.md) | Implemented source |
| Local server | `crates/qq-server/` | QQ server and store | [Headless](headless.md) | Implemented source |
| Provider credentials | `crates/qq-auth/`, `src/main.rs` | OS credential store | [Providers](providers.md) | Implemented source |
| External tools | `crates/qq-mcp/`, `crates/qq-harness/` | MCP host and QQ plan | [MCP](mcp.md) | Implemented source |

## Startup components

| Component | Source entrypoint | Storage or authority | Guide | Current basis |
| --- | --- | --- | --- | --- |
| Assistant gateway | Rust assistant `src/bin/qq-assistant-converse.rs` candidate | Assistant SQLite store plus QQ authority | [Assistant gateway](startup-assistant.md) | Candidate source |
| Mac client | `Sources/QQClientCore`, Mac client sources | Local client SQLite; QQ owns sessions | [Native modes](startup-native-modes.md) | Candidate source |
| iOS client | iOS client sources and shared core | Device storage; QQ remains host authority | [Native modes](startup-native-modes.md) | Candidate source |
| Retsu API gateway | Retsu API package and Worker source | Customer key, credits, usage | [API and billing](startup-api-billing.md) | Candidate or private source |
| Grip model backend | `grip serve` source design | Model process and metering sidecar | [Infrastructure](startup-infrastructure.md) | Planned deployment boundary |
| Observability | QQ tracing and proposed Sentry boundary | Privacy-safe diagnostics | [Observability](startup-observability.md) | Planned composition |

The assistant and native rows describe separate private repositories. Access to
those repositories is required. Their local checks do not prove provider,
device, hosted, or customer acceptance.

## Read the map correctly

The guide links to source names rather than private transcripts or local host
paths. If a source row says candidate, read its acceptance section before
using it as a product claim. If a row says planned, do not document an
endpoint, credential flow, or deployment as available.
