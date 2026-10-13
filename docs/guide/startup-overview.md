# Startup systems overview

This guide is the landing page for the current QQ and Retsu startup systems.
Choose a task card, then read the source and evidence boundary on its page.

## Task cards

<nav class="startup-task-grid" aria-label="Startup systems tasks">
  <a class="startup-task-card" href="../startup-getting-started/">
    <span class="startup-task-kicker">Terminal</span>
    <strong>Install and run QQ</strong>
    <span>Start with the smallest local journey, then choose TUI, headless, server, or MCP.</span>
  </a>
  <a class="startup-task-card" href="../startup-qq-development/">
    <span class="startup-task-kicker">Contribute</span>
    <strong>Change QQ safely</strong>
    <span>Trace the source entrypoint, exercise the real path, review, adopt, or roll back.</span>
  </a>
  <a class="startup-task-card" href="../startup-assistant/">
    <span class="startup-task-kicker">Assistant</span>
    <strong>Understand gateway and nodes</strong>
    <span>Separate the Rust candidate, local journal, planned surfaces, and QQ authority.</span>
  </a>
  <a class="startup-task-card" href="../startup-native-modes/">
    <span class="startup-task-kicker">Native</span>
    <strong>Compare direct and assistant modes</strong>
    <span>Follow the shared client contract, embedded Tailscale source, and pairing gaps.</span>
  </a>
  <a class="startup-task-card" href="../startup-api-billing/">
    <span class="startup-task-kicker">API and billing</span>
    <strong>Trace account-backed extraction</strong>
    <span>Map Worker, D1, AccountLedger, backend reserve/settle, sessions, and Stripe boundaries.</span>
  </a>
  <a class="startup-task-card" href="../startup-data-models/">
    <span class="startup-task-kicker">Data and models</span>
    <strong>Follow dataset and model evidence</strong>
    <span>Use the platform2 local workflows and keep reused references separate from hosted claims.</span>
  </a>
  <a class="startup-task-card" href="../startup-infrastructure/">
    <span class="startup-task-kicker">Infrastructure</span>
    <strong>Separate current from selected</strong>
    <span>Compare Cloudflare and Modal source with the planned ECS, Temporal, and Postgres target.</span>
  </a>
  <a class="startup-task-card" href="../startup-observability/">
    <span class="startup-task-kicker">Recovery</span>
    <strong>Diagnose and recover</strong>
    <span>Record request identity, health, event state, uncertain effects, and bounded repair evidence.</span>
  </a>
</nav>

<p class="startup-task-links">More: <a href="../startup-feature-map/">full feature map</a> · <a href="../startup-mcp-acp/">protocol boundaries</a> · <a href="../startup-maintenance/">documentation maintenance</a></p>

## Source snapshots

These identities are the basis for this guide family. A source revision does
not prove deployment or customer acceptance.

| Area | Snapshot | Access and limit |
| --- | --- | --- |
| QQ | `9bae6937b50a013f652db8963ac3590103eb4b84`, origin `main` | Public source; [QQ source](https://github.com/retsu-AI/qq/tree/9bae6937b50a013f652db8963ac3590103eb4b84) |
| Retsu | `8d58080829231bb74e180c6de4e8f687c4606200`, origin `main`, 2026-10-09 fetched snapshot | Private repository access required; `b889cd9e531438e4c54b25aa5ff9dc281c5e6888` is historical and older auth/session source |
| Assistant Rust | `ebd0dbb39cdb63cdc5eb9968366a568c442289b8`, branch `codex/rust-assistant-slice` | Private candidate source; provider and hosted acceptance unverified |
| Assistant Mac | `a4a1d247a26c7019e4d4cec1ea84f3cd4ed309cd`, branch `feat/local-mac-assistant` | Private candidate source; native acceptance unverified |
| Assistant iOS | `a3ef908573b73fbc3fe35dd00336fa4ef2eab21a`, branch `feat/ios-paired-continuation` | Private candidate source; device and remote acceptance unverified |
| Data platform | `cea2df0f756b5bb3523de168f7e488a05b932ffc`, branch `codex/data-review-experiment-20261013` | Private candidate source; hosted product unverified |

QQ links point to public source. Private repository links require access and
are omitted where a stable public URL does not exist. No page publishes local
host paths, raw receipts, account identifiers, credentials, or private chats.

## Status terms

**Implemented source** means the reviewed source contains the behavior.
**Candidate source** means a separate source unit still needs composition or
independent acceptance. **Planned** means the reviewed source names work that
is not implemented in this snapshot. **Runtime unverified** means source and
local checks do not prove a live provider, host, device, deployment, payment,
or customer journey.

The detailed QQ pages remain authoritative for released terminal behavior.
Use [Install](install.md), [Providers](providers.md), [CLI](cli.md), and
[Troubleshooting](troubleshooting.md) instead of copying those procedures.
