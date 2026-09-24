# Progress ledgers

One ledger per active plan, plus root, decisions, and gate evidence. Rules are
in [`../workflow.md` § 3](../workflow.md#3-ledgers): one writer per file,
current-state table on top, append-only dated entries below, raw evidence
under `target/qq-perf/` and never committed.

| File | Owner | Covers |
| --- | --- | --- |
| [`speed-first.md`](./speed-first.md) | agent on the speed-first plan | H and HC slices; two open quiet-host recordings; Phases 7–9 when gated |
| [`terminal-bench.md`](./terminal-bench.md) | agent on the readiness plan | R6–R8 and the Terminal-Bench evaluation program |
| [`supervised-delegation.md`](./supervised-delegation.md) | agent on the delegation plan | D6b paired evaluation and default decisions |
| [`multi-surface-clients.md`](./multi-surface-clients.md) | agent on the multi-surface plan | W, S, U, D, M slices and the tracer-bullet gate |
| [`tool-layer.md`](./tool-layer.md) | agent on the tool-layer plan | T1–T14 built-in tool slices and the A0–A5 ablation |
| [`run-reliability.md`](./run-reliability.md) | agent on the run-reliability plan | RR1–RR12: turn recovery, mid-run compaction, checkpoint tolerance, admission validation, tool leniency |
| [`delegated-approval.md`](./delegated-approval.md) | closed 2026-09-24 (receipt) | DA1–DA6 shipped: reviewer denial is final, two clocks, `approval_delegate`, exact delegate grants, `jev_approval` (ADR-0041), `/delegate` and delegate identity on the wire (protocol 28). Acceptance 3 (one week of use) to be recorded |
| [`onboarding-ux.md`](./onboarding-ux.md) | agent on the onboarding plan | OB0–OB11: user guide, startup error text, TUI without model/credential, doctor, init, install paths, trust prompt, docs-truth CI |
| [`security-kernel.md`](./security-kernel.md) | agent on the security-kernel plan | SK0–SK3: ADR-0042, hash-linked audit export, MCP pinning/quarantine, approval ledger and bound commit |
| [`root.md`](./root.md) | lead | Shared-file changes, dependency and toolchain bumps, ADR number allocation, cross-plan requests; ADR-0035 reserved and accepted locally for GitHub #83 |
| [`decisions-needed.md`](./decisions-needed.md) | anyone appends; lead resolves | Open questions with the conservative default taken |
| [`g-phase-5b.md`](./g-phase-5b.md), [`g-phase-6.md`](./g-phase-6.md), `g-<name>.md` | lead | Phase gate runs on `main` with exact SHA, commands, counts, and what was not tested |

Status vocabulary: `Planned` · `In progress` · `In review` · `Shipped (sha)`
· `Blocked (reason)` · `Dropped (reason)`.

Read the ledger before the plan when you want to know what is happening; read
the plan when you want to know what is intended. Open-work status lives in
Linear project `qq` (milestones per plan); ledgers record receipts once a
slice ships and are reconciled to Linear, not the reverse.
