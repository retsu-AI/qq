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
| [`speed-first-extensible-agent-harness.md`](./speed-first-extensible-agent-harness.md) | Backend plan: compiled plan, protocol, extension lanes, hot path, perf gates. Phases 0–6 closed; Phases 7–9 gated; two quiet-host recordings open |
| [`terminal-bench-readiness.md`](./terminal-bench-readiness.md) | Harness reliability and Terminal-Bench program; R6–R8 open (R6 candidate designs moved to `tool-layer.md`) |
| [`tool-layer.md`](./tool-layer.md) | Slim, safe, token-efficient built-ins. T1–T8 and T12 shipped (v0.1.0, #45, #49); open: T9 `fetch`, T11 `view_image`, T13 ablations, T14 `select_tools` index; T10 `terminal` gated |
| [`supervised-delegation.md`](./supervised-delegation.md) | Continuation, roster, supervised children, audit; D6b open |
| [`multi-surface-clients.md`](./multi-surface-clients.md) | Web, desktop, and mobile clients over many headless servers. W1, W2, S1, S3 shipped; open: S2 enrollment, S4 exposure, W3, then U/D/M |
| [`run-snapshots.md`](./run-snapshots.md) | Proposed: reversible mutating-run state |
| [`lsp-diagnostics.md`](./lsp-diagnostics.md) | Proposed: diagnostics integration |
| [`templates/`](./templates/) | Slice header, pre-flight, receipt, PR body; review checklist |
| [`progress/`](./progress/) | One ledger per plan, root ledger, decisions needed, gate evidence |

## Priority

| # | Next slice | Plan | Why now |
| ---: | --- | --- | --- |
| 1 | T9 `fetch` (`Network` class; completes ADR-0021) | `tool-layer.md` | T8 shipped `Interactive` (#49); `Network` completes the approval lattice the classifier introduced, and `fetch` is the last catalog gap from the 2026-09 audit |
| 2 | T13 ablation harness (A0–A4s arms) | `tool-layer.md` | Every tool-layer target (≥25 % fewer calls, ≥35 % fewer tokens) is unmeasured until this runs; it also feeds R6's evidence gate for T10 and H10 |
| 3 | D6b paired evaluation (paid runs) and the default decisions it feeds | `supervised-delegation.md` | Decides delegation depth and worker-model defaults with evidence |
| 4 | Multi-surface S2 enrollment (ADR-0015) and S4 exposure (ADR-0016); then W3 | `multi-surface-clients.md` | W1/W2/S1/S3 shipped; a remote client is blocked on authentication |
| 5 | T14 `select_tools` index; T11 `view_image` | `tool-layer.md` | Small; T11 needs the provider image content block |
| 6 | Phase 7 — H10 process sandbox | `speed-first-…` | Gated on R6 (T13 evidence, T10 decision) and a platform threat model |
| 7 | Phase 8 — H11 product adapters; Phase 9 — H12 qualification | `speed-first-…` | H11 needs a real consumer; H12 closes the story |
| — | Quiet-host recordings: Phase 5a H0 tail comparison; H20 eight-stream p95 then the 50→20 ms budget | `speed-first-…` | Implemented; tails not repeatable on the shared host; retained, not waived |
| — | Seven H22 deferrals (`StaticHttpAuth`, headless writer, config parse-once, reviewer via `PlanCache`, run-loop enums, args-parse-once, `Arc` calls) | `speed-first-…` § Bundled Fixes | Each is its own slice when that code is next opened |
| — | Run snapshots, LSP diagnostics | proposed plans | No scheduled slice |

## Ownership

| Concern | Owner |
| --- | --- |
| Compiled plan, protocol contract, extension lanes, store/provider hot path, perf gates and budgets, headless-contract sequencing (HC1–HC4) | `speed-first-extensible-agent-harness.md` |
| Tool-contract ablations, terminal, sub-agent economics, Terminal-Bench evaluation program, remaining warm-path candidates | `terminal-bench-readiness.md` |
| Built-in tool contracts, output bounding and spill, shell classification, `exec`/`fetch`/`ask_user`/`terminal`, `@` mentions | `tool-layer.md` |
| Continuation on truncation, delegation roster, supervised write children, final-answer audit, paired evaluation | `supervised-delegation.md` |
| Web, desktop, mobile clients; remote server readiness (identity, enrollment, CORS, TLS, workspace catalog) | `multi-surface-clients.md` |
| Reversible mutating-run state | `run-snapshots.md` |
| Diagnostics integration | `lsp-diagnostics.md` |
| Reference audit of Codex, OpenCode, Pi, fx, and the Hermes boundary | [`../design/harness-audit-2026-08.md`](../design/harness-audit-2026-08.md) (research, not a plan) |
| Per-feature harness catalog and ranked QQ gaps | [`../design/harness-catalog-2026-09.md`](../design/harness-catalog-2026-09.md) (research, not a plan) |
| Shared files, dependency and toolchain bumps, ADR numbering | [`progress/root.md`](./progress/root.md) |

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
