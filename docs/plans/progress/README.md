# Progress ledgers

One ledger per active plan, with the same file name as the plan, plus root,
decisions, and gate evidence. Rules are in
[`../workflow.md` § 3](../workflow.md#3-ledgers): one writer per file,
current-state table on top, append-only dated entries below, raw evidence
under `target/qq-perf/` and never committed. When a plan is fully shipped its
ledger is deleted with it; the receipts stay in Git history.

| File | Owner | Covers |
| --- | --- | --- |
| [`autonomous-core.md`](./autonomous-core.md) | agent on the autonomous-core plan | AP0–AP5 progress track; AC0–AC16: soak harness, reset scopes, continuation, loop guard, `qq-harness`, tool features, store write amplification (AC7–AC9 moved to goals) |
| [`goals.md`](./goals.md) | agent on the goals plan | G0–G5: goal state and protocol, goal in runs, driver, completion check, surfaces, evidence |
| [`compaction.md`](./compaction.md) | agent on the compaction plan | CX0–CX5: narrative plus rendered record, cache-aligned summarizer, prune watermark, compacting activity, live qualification |
| [`run-reliability.md`](./run-reliability.md) | agent on the run-reliability plan | RR1–RR12: turn recovery, checkpoint tolerance, admission validation, tool leniency (RR12's loop result moved to autonomous-core AC4) |
| [`tool-layer.md`](./tool-layer.md) | agent on the tool-layer plan | T1–T17 built-in tool slices and the A0–A5 ablation |
| [`token-efficiency.md`](./token-efficiency.md) | agent on the token-efficiency plan | TE0–TE8 and links to existing evaluation owners |
| [`guide-expansion.md`](./guide-expansion.md) | agent on the guide-expansion plan | GE0 guide corrections; GE10 concepts, GE9 workflows; GE1–GE8: agents, sessions, skills, environment, keybindings, server, enterprise, changelog pages |
| [`tui-redesign.md`](./tui-redesign.md) | TUI lane | U0–U9 and L1–L4 |
| [`jev.md`](./jev.md) | agent on the Jev plan | JV0–JV13; carries the J1–J9, RR3, DA5 and 2026-09-25 audit history |
| [`multi-surface-clients.md`](./multi-surface-clients.md) | agent on the multi-surface plan | W, S, U, D, M slices and the tracer-bullet gate |
| [`supervised-delegation.md`](./supervised-delegation.md) | agent on the delegation plan | D6b paired evaluation and default decisions |
| [`terminal-bench.md`](./terminal-bench.md) | agent on the Terminal-Bench plan | R6–R8 and the Terminal-Bench evaluation program |
| [`speed-first.md`](./speed-first.md) | agent on the speed-first plan | H and HC slices; two open quiet-host recordings; Phases 7–9 when gated |
| [`root.md`](./root.md) | lead | Shared-file changes, dependency and toolchain bumps, ADR number allocation, cross-plan requests, harness-comparison findings (F-rows) |
| [`decisions-needed.md`](./decisions-needed.md) | anyone appends; lead resolves | Open questions with the conservative default taken |
| [`g-phase-5b.md`](./g-phase-5b.md), [`g-phase-6.md`](./g-phase-6.md) | lead | Phase gate runs on `main` with exact SHA, commands, counts, and what was not tested (`g-<phase>.md`) |

Status vocabulary: `Planned` · `In progress` · `In review` · `Shipped (sha)`
· `Blocked (reason)` · `Dropped (reason)`.

Read the ledger before the plan when you want to know what is happening; read
the plan when you want to know what is intended. Open-work status lives in
Linear project `qq` (milestones per plan); ledgers record receipts once a
slice ships and are reconciled to Linear, not the reverse.
