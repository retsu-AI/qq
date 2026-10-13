# Startup systems overview

This guide is the landing page for the current QQ and Retsu startup systems.
Choose a task card, then read the source and evidence boundary on its page.

## Task cards

| I need to… | Start here | What the page gives you |
| --- | --- | --- |
| Install and run QQ | [Get started with the stack](startup-getting-started.md) | The smallest terminal path and its current source boundary |
| Change QQ safely | [Develop QQ](startup-qq-development.md) | Source entrypoints, checks, review, adoption, and rollback |
| Find a feature | [Feature map](startup-feature-map.md) | Component ownership, exact source links, and status |
| Understand the assistant | [Assistant gateway and nodes](startup-assistant.md) | Rust gateway, node, memory, and QQ authority boundaries |
| Compare native modes | [Direct and assistant modes](startup-native-modes.md) | Current client source, embedded Tailscale, and open acceptance |
| Operate API or billing | [API and billing](startup-api-billing.md) | Cloudflare Worker, D1, AccountLedger, Modal, and Stripe flow |
| Operate data and models | [Data and models](startup-data-models.md) | Grip and platform2 source workflows, with hosted limits |
| Operate staging or production | [Infrastructure](startup-infrastructure.md) | Current Cloudflare and Modal source versus the selected AWS target |
| Diagnose a failure | [Observability](startup-observability.md) and [Recovery](startup-recovery.md) | Bounded signals and explicit recovery decisions |
| Connect tools or protocols | [MCP and ACP](startup-mcp-acp.md) | QQ MCP client, helper server, and ACP status |
| Edit these pages | [Maintain the documentation](startup-maintenance.md) | Source links, status labels, and site checks |

## Source snapshots

These identities are the basis for this guide family. A source revision does
not prove deployment or customer acceptance.

| Area | Snapshot | Access and limit |
| --- | --- | --- |
| QQ | `9bae6937b50a013f652db8963ac3590103eb4b84`, origin `main` | Public source; [QQ source](https://github.com/retsu-AI/qq/tree/9bae6937b50a013f652db8963ac3590103eb4b84) |
| Retsu | `b889cd9e531438e4c54b25aa5ff9dc281c5e6888`, local `main`, 2026-09-30 snapshot | Private repository access required; local snapshot was 65 commits behind its remote at review |
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
