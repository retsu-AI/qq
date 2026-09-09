# Ledger — speed-first extensible agent harness

Plan: [`../speed-first-extensible-agent-harness.md`](../speed-first-extensible-agent-harness.md).
Only the agent working this plan edits this file. Current state on top;
dated entries appended below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| 5a-accept | Full version-4 H0 comparison on a quiet host | Planned | | Baseline `1c08cef`, candidate `main`. Prior recordings on the shared host: A/A fails the same tail gates as A/B; retained, not waived |
| 5a-windows | Full native Windows workspace run | Planned | | Targeted `windows-teardown` CI job passes; full qualification not claimed |
| H20 | Wake-driven control admission; delete 13 `sleep(1 ms)` loops; ≤20 ms output gap | Planned | | **Next.** Pre-change attribution baseline in `893e582`: output commit gap p95 37.6 ms, queued service p95 31.2 ms. ADR-0011 reserved |
| H21.1 | Behavioral settlement: `RunIdentity`, `RunSettlement`, `PersistenceFault`, teardown-before-terminal structural | Planned | | After H20. ADR-0012 reserved |
| H27 | Superseded-generation accounting, atomic refresh admission, guard reclamation | Planned | | Pinned LRU and admission already exist (`src/plan.rs`) |
| H28 | Typed context-source capacity error; sources in descriptor | Planned | | `DESCRIPTOR_VERSION` bump. ADR-0013 reserved |
| H22.1 | Correctness bundle: delete ~37 `notify(` sites, stored-kind pruning, MCP permit ordering | Planned | | |
| H18 | `Arc<Vec<Message>>`, prompt prefix, `RawValue` schemas | Planned | | Add `provider_encode` bench first |
| H19 | SSE framing, conditional | Planned | | Add `sse_decode` bench first; no-change decision acceptable |
| H21.2 | Mechanical `sessions.rs` split | Planned | | After HC3 behavioral changes; separate commit |
| H22.2 | Structural bundle: `COMMAND_ROUTES`, `Box<SessionSummary>`, `StaticHttpAuth`, config/auth load, TUI | Planned | | |
| HC1 | `--correlation`, `--session`, `u32` turns, model-less `config check` | Planned | | `PROTOCOL_VERSION` bump. Parallel worktree OK |
| HC3 | `--output-schema`, repair turns, `final_output` | Planned | | Before H21.2. ADR-0014 reserved |
| HC4 | Headless golden fixtures | Planned | | After HC1–HC3 |
| H10 / H11 / H12 | Sandbox / adapters / qualification | Planned | | Gated; see plan |

Shipped before this ledger existed (see the plan's Completed Phases table):
H0–H9, H13–H17, H23–H26, HC2. Last shipped: `893e582` (2026-09-07).

## Entries

### 2026-09-08 — ledger opened

Plan compressed from 2,582 to 774 lines; reference audit moved to
`docs/design/harness-audit-2026-08.md`; ADR-0001–0010 backfilled. Verified
against source: H20 has 13 remaining overload loops (`execution.rs` ×9,
`scheduler.rs` ×3, `subagents.rs` ×1) plus 50 ms cancel polls in `qq-mcp`,
`hosts/embedded.rs`, `tools/shell.rs`; `control_slots` exists but ordinary
admission still `try_acquire`s. H21 types absent; `sessions/` partly split
(`approvals`, `context`, `execution`, `feed`, `runtime`, `scheduler`, `store`,
`subagents`). H18: `messages: Vec<Message>`, no `prompt_prefix`, schema is
`Value`. H27: pinned LRU present, superseded accounting absent. H28: ninth
source silently ignored at `lib.rs:626`. HC1/HC3/HC4 not started. Versions:
protocol 16, capabilities 1, descriptor 5, schema 25, H0 fixture 4.

Corrected plan SHAs that did not exist in this repository: Phase 1 `5bb1471`
(was `8ccba84`), Phase 2 `2d2ba3b` (was `2375928`), Phase 3 `dfaebb9` (was
`27afe89`), Phase 4 `f02cfc9` (was `5f48fd6`); HC2 and the H20 attribution
baseline are inside the `893e582` squash.

Shipped: none. In progress: none. Blocked: none.
