# Ledger — desktop app, projects, and parallel autonomous work

Plan: [`../desktop.md`](../desktop.md). Linear:
[ENG-1073](https://linear.app/retsu-ai/issue/ENG-1073). Only the agent
working this plan edits this file. Current state on top; dated entries
appended below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| D0 | Design prototype and review | Planned | `feat/desktop` | Gates Phase 0 |
| A0 | `apps/` scaffold, CI matrix, `apps` gate | Planned | `feat/desktop` | ADR-0017, ADR-0018 |
| C1 | Attention ranking in `qq-client` | Planned | | |
| C2 | View core and patch channels | Planned | | |
| C3 | Generated TypeScript types and drift check | Planned | | |
| K1 | Projects | Planned | | Protocol and schema bump |
| K2 | Attention inputs, read state, error reasons | Planned | | Protocol bump |
| A1–A2 | Local desktop, Inbox | Planned | `feat/desktop` | |
| K3–K7 | Git state, worktrees, change sets, fork, fan-out | Planned | | ADR-0064; K4 second review |
| A3 | Board, New task, panels, review | Planned | `feat/desktop` | |
| K8–K9 | Tasks, coordinator, integration actions | Planned | | ADR-0065 reserved |
| A4 | Coordinator view | Planned | `feat/desktop` | |
| A5 | Remote servers in the app | Planned | `feat/desktop` | Needs W3, S2, S4, S5 |
| A6, R1 | Polish and release | Planned | `feat/desktop` | |
| A7–A8 | Web and mobile | Planned | | |
| A9 | Merge `feat/desktop` to `main` | Planned | | |

## Entries

### 2026-10-18 — plan opened

Plan, ADR-0017, ADR-0018, and ADR-0064 drafted from a review of the engine,
the shared client crate, the multi-surface plan, and the unmerged desktop
prototype (#314). The prototype is not carried forward as code; its findings
(native client core works over the existing server; full-state emission per
frame and an unbounded command channel must change) shaped C2. ADR-0017 and
ADR-0018 take the numbers the multi-surface plan reserved; ADR-0064 and
ADR-0065 reserved in `root.md` (0063 is held by #329). The multi-surface plan
keeps W3 and S2–S6; its U, D, and M phases are superseded here.

Shipped: none. In progress: plan review. Blocked: none.
