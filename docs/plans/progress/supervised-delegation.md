# Ledger — supervised delegation

Plan: [`../supervised-delegation.md`](../supervised-delegation.md).
Only the agent working this plan edits this file. Current state on top;
dated entries appended below, newest last.

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| D1 | Bounded continuation on output truncation | Shipped (`e074f89`) | | Protocol 16, schema 22 |
| D2 | Child accounting and authority repair | Shipped (`7a0a1e5`; H24 refresh in `893e582`) | | Per-admission remaining budgets, deadline carry, descendant spend |
| D3 | Delegation roster | Shipped (`c8bf342`) | | Descriptor 4, prompt 10 |
| D4a | Supervised write children at depth one | Shipped (`9ddbbb8`; H23 ownership in `1e6a901`, `f482b37`) | | |
| D4b | Configurable depth to three | Shipped (`9aa30e8`) | | Schema 23 |
| D5 | Heuristic final-answer audit | Shipped (`a1939d2`) | | Schema 24 |
| D6a | Compare command, arm stamping, reasoning tokens | Shipped (`428af0a`, `66f3aba`) | | Runbook `benchmarks/arms/README.md` |
| D6b | Paired runs and the default decisions they feed | Planned | | Needs spend; decides depth and worker-model defaults |

## Entries

### 2026-09-08 — ledger opened

D1–D5 and D6a shipped 2026-09-03; H23/H24 from the speed-first plan landed
through this plan's D4 and D2 contracts on 2026-09-04/05. D6b is the only
open slice and is blocked on paid runs.

Shipped: none new. In progress: none. Blocked: D6b (spend).
