# Progress ledgers

One ledger per active plan, plus root, decisions, and gate evidence. Rules are
in [`../workflow.md` § 3](../workflow.md#3-ledgers): one writer per file,
current-state table on top, append-only dated entries below, raw evidence
under `target/qq-perf/` and never committed.

| File | Owner | Covers |
| --- | --- | --- |
| [`speed-first.md`](./speed-first.md) | agent on the speed-first plan | H and HC slices; Phases 5a acceptance, 5b, 6–9 |
| [`terminal-bench.md`](./terminal-bench.md) | agent on the readiness plan | R6–R8 and the Terminal-Bench evaluation program |
| [`supervised-delegation.md`](./supervised-delegation.md) | agent on the delegation plan | D6b paired evaluation and default decisions |
| [`multi-surface-clients.md`](./multi-surface-clients.md) | agent on the multi-surface plan | W, S, U, D, M slices and the tracer-bullet gate |
| [`root.md`](./root.md) | lead | Shared-file changes, dependency and toolchain bumps, ADR number allocation, cross-plan requests |
| [`decisions-needed.md`](./decisions-needed.md) | anyone appends; lead resolves | Open questions with the conservative default taken |
| `g-<name>.md` | lead | Phase gate runs on `main` with exact SHA, commands, counts, and what was not tested |

Status vocabulary: `Planned` · `In progress` · `In review` · `Shipped (sha)`
· `Blocked (reason)` · `Dropped (reason)`.

Read the ledger before the plan when you want to know what is happening; read
the plan when you want to know what is intended.
