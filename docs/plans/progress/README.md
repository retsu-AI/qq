# Progress and evidence

Current acceptance owners: [plan index](../README.md). New receipts belong
to the current owner's ledger; old slice IDs and tracker issues remain valid.

| Current ledger | Scope |
| --- | --- |
| [reliability](reliability.md) | AC/AP/AB/CX remaining delivery and live/resource qualification |
| [developer experience](developer-experience.md) | GE guide work, AF feedback/audit/evaluation |
| [goals](goals.md) | G0/G2–G5 delivery/check authority |
| [tool layer](tool-layer.md) | Built-in tools and T13 ablations |
| [token efficiency](token-efficiency.md) | TE accounting/experiments |
| [Terminal-Bench](terminal-bench.md) | R6–R8, D6b economics and TB-pilot |
| [speed first](speed-first.md) | Backend residuals and gated extensions |
| [TUI](tui-redesign.md) | Remaining TUI work |
| [TUI focus](tui-focus.md) | F1–F6 sessions-first and selection slices |
| [Jev](jev.md) | JV precision/lifecycle/qualification |
| [multi-surface](multi-surface-clients.md) | Remote readiness and clients |
| [v0.1.7 release](release-0.1.7.md) | Publication verification receipt still pending; bump/guide pins merged |
| [auth providers](auth-providers.md) | AP slices: qq-auth/provider credential refactor |
| [root](root.md) | Coordination, shared files, ADR allocation |
| [decisions](decisions-needed.md) | Decisions and conservative defaults |
| [performance gates](performance-gates.md) | Current measurement rules and failed/unqualified receipts |

Historical ledgers retained for ADR/research and baseline evidence:
[autonomous core](autonomous-core.md), [action bias](action-bias.md),
[compaction](compaction.md), [agent feedback](agent-feedback.md),
[guide expansion](guide-expansion.md), [delegation](supervised-delegation.md).
Their old status tables are snapshots, not active assignments. Phase gate
receipts [5b](g-phase-5b.md) and [6](g-phase-6.md) remain evidence, not proof
that their carried tail gates passed.

Follow [workflow](../workflow.md#3-ledgers): one writer per active ledger,
current state above append-only dated receipts, raw logs under target and
out of Git. Verify live assignments in Linear; this index does not assert
remote issue/PR status.
