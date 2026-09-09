# Ledger — Terminal-Bench readiness

Plan: [`../terminal-bench-readiness.md`](../terminal-bench-readiness.md).
Only the agent working this plan edits this file. Current state on top;
dated entries appended below, newest last.

Phases 1–5 (durable headless run, authoritative context, task-completion
contract, linear/fair streaming R4, resolved model and spend R5) are complete
and qualified; receipts are in the plan. Remaining work is evaluation-gated.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| R6-search | Ignore-aware search candidate: fixtures, paired evaluation, keep/reject | Planned | | Needs spend for paired runs |
| R6-patch | Patch-style edit candidate | Planned | | |
| R6-terminal | Persistent terminal/process contract candidate | Planned | | Gates speed-first H10 |
| R7 | Sub-agent economics and provider-aware scheduling | Planned | | H23/H24 shipped the ownership and budget repairs; remaining items are worker-model defaults and queue-wait recording |
| R8 | Remaining warm-path candidates: credential-lease caching, MCP bounds, prompt-cache determinism | Planned | | Retry, shared messages, and encode bench handed to speed-first H14 (done) and H18 |
| TB-pilot | Credentialed Terminal-Bench pilot beyond the first smoke | Planned | | First smoke recorded `9e275be`; Harbor adapter `990df3e`, `af6ed7e`, `9d38c08`, `b80c919` |

## Entries

### 2026-09-08 — ledger opened

No R6–R8 slice in progress. The last landed work on this plan was the
Terminal-Bench pilot tooling (`feat/terminal-bench-pilot`, merged `723b7eb`).

Shipped: none since the pilot. In progress: none. Blocked: R6 paired
evaluations and TB-pilot need paid model runs.
