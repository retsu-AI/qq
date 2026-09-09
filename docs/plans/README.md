# Plans

Active plans only. Shipped plans are deleted; their receipts live in Git
history and their durable contracts in [`../design/`](../design/). Read
[`../design/architecture.md`](../design/architecture.md) before changing
system boundaries and [`workflow.md`](./workflow.md) before starting a slice.

## How to use this directory

| If you are… | Read, in order |
| --- | --- |
| deciding what to work on | this file § Priority, then the plan's status block |
| picking up a slice | [`workflow.md`](./workflow.md), the plan's task index and phase section, [`templates/slice.md`](./templates/slice.md), the plan's ledger in [`progress/`](./progress/) |
| reviewing a PR | [`templates/review-checklist.md`](./templates/review-checklist.md), the slice section, the ledger receipt |
| checking what is in flight | [`progress/README.md`](./progress/README.md) and the plan's ledger |
| recording a decision | [`../adr/README.md`](../adr/README.md); reserve the number in [`progress/root.md`](./progress/root.md) |

## Files

| File | Purpose |
| --- | --- |
| [`workflow.md`](./workflow.md) | Slice protocol, ledger rules, review, escalation, dispatch skeletons |
| [`speed-first-extensible-agent-harness.md`](./speed-first-extensible-agent-harness.md) | Backend plan: compiled plan, protocol, extension lanes, hot path, perf gates; Phases 5a–9 and HC1–HC4 |
| [`terminal-bench-readiness.md`](./terminal-bench-readiness.md) | Harness reliability and Terminal-Bench program; R6–R8 open |
| [`supervised-delegation.md`](./supervised-delegation.md) | Continuation, roster, supervised children, audit; D6b open |
| [`run-snapshots.md`](./run-snapshots.md) | Proposed: reversible mutating-run state |
| [`lsp-diagnostics.md`](./lsp-diagnostics.md) | Proposed: diagnostics integration |
| [`templates/`](./templates/) | Slice header, pre-flight, receipt, PR body; review checklist |
| [`progress/`](./progress/) | One ledger per plan, root ledger, decisions needed, gate evidence |

## Priority

| # | Next slice | Plan | Why now |
| ---: | --- | --- | --- |
| 1 | Phase 6 — H20 control admission first, then behavioral H21/H27/H28 and correctness H22, then H18, measured H19, and mechanical consolidation | `speed-first-…` | Closes the carried eight-stream service-gap gate (23–28 ms vs the 20 ms target), removes the 13 `sleep(1 ms)` overload loops, repairs cache accounting and context-source identity, then moves the 1 MiB heap and cold `plan_for` gates |
| 2 | Phase 5b — HC1, HC3, HC4 headless contract (parallel worktree) | `speed-first-…` and [`../design/headless-contract.md`](../design/headless-contract.md) | HC2 shipped; HC3 must land before the mechanical `sessions.rs` split |
| 3 | D6b paired evaluation (paid runs) and the default decisions it feeds | `supervised-delegation.md` | Decides delegation depth and worker-model defaults with evidence |
| 4 | R6 tool tournament and terminal; R7 sub-agent economics; R8 remaining warm-path candidates | `terminal-bench-readiness.md` | Evaluation-gated; R6 feeds H10 |
| 5 | Phase 7 — H10 process sandbox | `speed-first-…` | Gated on R6 and a platform threat model |
| 6 | Phase 8 — H11 product adapters; Phase 9 — H12 qualification | `speed-first-…` | H11 needs a real consumer; H12 closes the story |
| — | Phase 5a quiet-host H0 tail acceptance; full Windows run | `speed-first-…` | Implemented; tails not repeatable on the shared host; retained, not waived |
| — | Run snapshots, LSP diagnostics | proposed plans | No scheduled slice |

## Ownership

| Concern | Owner |
| --- | --- |
| Compiled plan, protocol contract, extension lanes, store/provider hot path, perf gates and budgets, headless-contract sequencing (HC1–HC4) | `speed-first-extensible-agent-harness.md` |
| Tool-contract ablations, terminal, sub-agent economics, Terminal-Bench evaluation program, remaining warm-path candidates | `terminal-bench-readiness.md` |
| Continuation on truncation, delegation roster, supervised write children, final-answer audit, paired evaluation | `supervised-delegation.md` |
| Reversible mutating-run state | `run-snapshots.md` |
| Diagnostics integration | `lsp-diagnostics.md` |
| Reference audit of Codex, OpenCode, Pi, fx, and the Hermes boundary | [`../design/harness-audit-2026-08.md`](../design/harness-audit-2026-08.md) (research, not a plan) |
| Shared files, dependency and toolchain bumps, ADR numbering | [`progress/root.md`](./progress/root.md) |

Shipped and removed 2026-09-04: TUI rearchitecture and refinement, compaction,
model-reviewed approvals, read-only sub-agents (Phases A–C), provider
rearchitecture, client parity (Tiers 1–2), the proposed `qq-core` physical
extraction (superseded by D9), and the Terminal-Bench baseline-repair tranche
(folded into readiness Phase 6 gates).

## Conventions

- Slice IDs come from the plan's task index (`H20`, `HC3`, `R6-search`,
  `D6b`); split large tasks in the ledger as `H20.1`, `H20.2`.
- A plan's status block is derived from its ledger and updated in the same PR
  that ships the work, or the immediately following docs PR.
- Record a pre-change baseline for every named performance gate before the
  change lands ([`../runbooks/perf-recording.md`](../runbooks/perf-recording.md)).
- Phase sections follow one template: status, tasks, acceptance, receipt of at
  most about fifteen lines with commit SHAs. When a phase closes, collapse its
  section to one row in the plan's completed-phases table and write the gate
  file in `progress/`.
- Research that motivated a plan but does not change as work ships belongs in
  `../design/`, not in the plan.
- When a plan is fully shipped, move any durable contract into `../design/`,
  delete the plan, and update this index.
