# Progress ledgers

One ledger per active plan, plus root, decisions, and gate evidence. Rules are
in [`../workflow.md` § 3](../workflow.md#3-ledgers): one writer per file,
current-state table on top, append-only dated entries below, raw evidence
under `target/qq-perf/` and never committed.

| File | Owner | Covers |
| --- | --- | --- |
| [`token-efficiency.md`](token-efficiency.md) | agent on efficiency plan | TE0–TE8 and links to existing evaluation owners |
| [`speed-first.md`](./speed-first.md) | agent on the speed-first plan | H and HC slices; two open quiet-host recordings; Phases 7–9 when gated |
| [`terminal-bench.md`](./terminal-bench.md) | agent on the readiness plan | R6–R8 and the Terminal-Bench evaluation program |
| [`supervised-delegation.md`](./supervised-delegation.md) | agent on the delegation plan | D6b paired evaluation and default decisions |
| [`multi-surface-clients.md`](./multi-surface-clients.md) | agent on the multi-surface plan | W, S, U, D, M slices and the tracer-bullet gate |
| [`tool-layer.md`](./tool-layer.md) | agent on the tool-layer plan | T1–T14 built-in tool slices and the A0–A5 ablation |
| [`run-reliability.md`](./run-reliability.md) | agent on the run-reliability plan | RR1–RR12: turn recovery, checkpoint tolerance, admission validation, tool leniency (RR12's loop result moved to autonomous-core AC4) |
| [`autonomous-core.md`](./autonomous-core.md) | agent on the autonomous-core plan | AC0–AC16: soak harness, reset scopes, continuation, loop guard, `qq-harness`, tool features, store write amplification (AC7–AC9 moved to goals) |
| [`goals.md`](./goals.md) | agent on the goals plan | G0–G5: goal state and protocol, goal in runs, driver, completion check, surfaces, evidence |
| [`delegated-approval.md`](./delegated-approval.md) | closed 2026-09-24 (receipt) | DA1–DA6 shipped: reviewer denial is final, two clocks, `approval_delegate`, exact delegate grants, `jev_approval` (ADR-0041), `/delegate` and delegate identity on the wire (protocol 28). Acceptance 3 (one week of use) to be recorded |
| [`onboarding-ux.md`](./onboarding-ux.md) | closed 2026-09-25 (receipt) | OB0–OB12 shipped: user guide, actionable startup errors, TUI opens without model/credential/trust, doctor, init, install paths, first-session guidance, MCP credential degrade, docs-truth CI, docs site |
| [`guide-expansion.md`](./guide-expansion.md) | agent on the guide-expansion plan | GE0 guide corrections; GE9 workflows, GE10 concepts; GE1–GE8: agents, sessions, skills, environment, keybindings, server, enterprise, changelog pages |
| [`decision-models.md`](./decision-models.md) | agent on the decision-model plan | DM0–DM9, JV0–JV13, DX1–DX6; carries the J1–J9, RR3, DA5 and 2026-09-25 audit history (was `jev.md`) |
| [`mcp-pinning.md`](./mcp-pinning.md) | agent on ENG-939 | MP1: MCP tool-set pinning taken over from contributed #163/#165/#171 — dispatch-time enforcement, descriptor v10, `qq mcp inspect` (ADR-0046) |
| [`codex-device-auth.md`](./codex-device-auth.md) | agent on bounded ENG-809/ENG-791 auth slice | Opt-in Codex device authorization, reusing the existing protected credential and refresh lifecycle; offline qualification only |
| [`root.md`](./root.md) | lead | Shared-file changes, dependency and toolchain bumps, ADR number allocation, cross-plan requests; ADR-0035 reserved and accepted locally for GitHub #83 |
| [`decisions-needed.md`](./decisions-needed.md) | anyone appends; lead resolves | Open questions with the conservative default taken |
| [`g-phase-5b.md`](./g-phase-5b.md), [`g-phase-6.md`](./g-phase-6.md), `g-<name>.md` | lead | Phase gate runs on `main` with exact SHA, commands, counts, and what was not tested |

Status vocabulary: `Planned` · `In progress` · `In review` · `Shipped (sha)`
· `Blocked (reason)` · `Dropped (reason)`.

Read the ledger before the plan when you want to know what is happening; read
the plan when you want to know what is intended. Open-work status lives in
Linear project `qq` (milestones per plan); ledgers record receipts once a
slice ships and are reconciled to Linear, not the reverse.
