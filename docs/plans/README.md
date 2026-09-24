# Plans

Active plans only. Shipped plans are deleted; their receipts live in Git
history and their durable contracts in [`../design/`](../design/). Read
[`../design/architecture.md`](../design/architecture.md) before changing
system boundaries and [`workflow.md`](./workflow.md) before starting a slice.

## Tracker

Open work is tracked in Linear project `qq` (team `ENG`), one milestone per
plan below plus `Harness Audit`, `Evaluation Program`, and `Decisions`.
Ledgers in [`progress/`](./progress/) hold receipts and gate evidence; when
they disagree with Linear on status, Linear is current. Reserve ADR numbers
in [`progress/root.md`](./progress/root.md) as before.

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
| [`speed-first-extensible-agent-harness.md`](./speed-first-extensible-agent-harness.md) | Backend plan, collapsed to what is open: two quiet-host recordings, seven H22 deferrals, Phases 7–9 gated. Shipped design lives in `architecture.md` § Extension Contract and § Performance Discipline |
| [`onboarding-ux.md`](./onboarding-ux.md) | From `git clone` to a working agent in one minute: user guide, actionable startup errors, TUI opens without a model or credential, `qq doctor`, `qq init`, install script/brew/nix, in-TUI trust prompt, docs-truth CI. OB0 in review; OB1–OB11 open |
| [`run-reliability.md`](./run-reliability.md) | Sessions finish: turn-level recovery and `Paused`, reactive overflow and un-wedged admission, tolerant checkpoint, admission-time slash validation, lenient tool arguments. RR1–RR12 open; from the 2026-09-21 audit |
| [`terminal-bench-readiness.md`](./terminal-bench-readiness.md) | Harness reliability and Terminal-Bench program; R6–R8 open (R6 candidate designs moved to `tool-layer.md`) |
| [`tool-layer.md`](./tool-layer.md) | Slim, safe, token-efficient built-ins. T1–T9 and T12 shipped (v0.1.0, #45, #49, #50); open: T11 `view_image`, T13 ablations, T14 `select_tools` index; T10 `terminal` gated |
| [`supervised-delegation.md`](./supervised-delegation.md) | Continuation, roster, supervised children, audit; D6b open |
| [`multi-surface-clients.md`](./multi-surface-clients.md) | Web, desktop, and mobile clients over many headless servers. W1, W2, S1, S3 shipped; open: S2 enrollment, S4 exposure, W3, then U/D/M |
| [`run-snapshots.md`](./run-snapshots.md) | Proposed: reversible mutating-run state |
| [`mid-run-compaction.md`](./mid-run-compaction.md) | Compact and continue one run at a safe turn boundary (audit F03, ENG-793). MRC-0..3 shipped (#92, ADR-0039); open: MRC-4 surfaces, MRC-5 live evidence |
| [`lsp-diagnostics.md`](./lsp-diagnostics.md) | Proposed: diagnostics integration |
| [`security-kernel.md`](./security-kernel.md) | Axiom fail-closed guarantees as library code inside `qq-core`/`qq-mcp` (ADR-0042): hash-linked audit export (SK1), MCP tool-set pinning and quarantine (SK2), approval ledger and bound commit (SK3). SK0 in review |
| [`templates/`](./templates/) | Slice header, pre-flight, receipt, PR body; review checklist |
| [`progress/`](./progress/) | One ledger per plan, root ledger, decisions needed, gate evidence |

## Priority

| # | Next slice | Plan | Why now |
| ---: | --- | --- | --- |
| 0 | RR1–RR3 (checkpoint tolerance, slash admission, Jev outcome), then RR4 turn recovery | [`run-reliability.md`](./run-reliability.md) | 27 % of real prompt runs fail and 73 % of those are harness decisions on recoverable situations; 8.5 h of completed work discarded in the sampled store. Blocks daily use |
| 1 | OB1–OB2 (TUI opens without a model / credential), then OB6 install paths | [`onboarding-ux.md`](./onboarding-ux.md) | Every new user hits the first minute; the audit found the TUI exits or opens blank where every reference harness asks. Install today is a manual archive download |
| 2 | Live qualification of the context-usability stack | — (one manual run; record in `progress/root.md`) | C1–C6 shipped (#56–#64) on fixtures only. Confirm on a real long session: `cache_read_input_tokens > 0` on turn 2 of an Anthropic/Bedrock session, and a ~700 KB transcript on a 200k model sends and compacts |
| 3 | Harness-audit findings F03–F28 triage | [`../design/harness-scale-audit-2026-09-16.md`](../design/harness-scale-audit-2026-09-16.md) § Proposed work order | F01/F02/F14 shipped; the remaining findings have no owning plan yet. Largest: true mid-run summarization (F03; C2 stubs stale reads but defers the summarizer cutoff) |
| 4 | T13 ablation harness (A0–A4s arms) | `tool-layer.md` | Every tool-layer target (≥25 % fewer calls, ≥35 % fewer tokens) is unmeasured until this runs; it also feeds R6's evidence gate for T10 and H10 |
| 5 | D6b paired evaluation (paid runs) and the default decisions it feeds | `supervised-delegation.md` | Decides delegation depth and worker-model defaults with evidence; audit default flipped to `off` in C3 pending B1 |
| 6 | Multi-surface S2 enrollment (ADR-0015) and S4 exposure (ADR-0016); then W3 | `multi-surface-clients.md` | W1/W2/S1/S3 shipped; a remote client is blocked on authentication |
| 7 | T14 `select_tools` index; T11 `view_image` | `tool-layer.md` | Small; T11 needs the provider image content block |
| 8 | Phase 7 — H10 process sandbox | `speed-first-…` | Gated on R6 (T13 evidence, T10 decision) and a platform threat model |
| 9 | Phase 8 — H11 product adapters; Phase 9 — H12 qualification | `speed-first-…` | H11 needs a real consumer; H12 closes the story |
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
| Mid-run compaction and continuation | `mid-run-compaction.md` |
| Diagnostics integration | `lsp-diagnostics.md` |
| First-run and configuration UX, user guide, install paths, community files | `onboarding-ux.md` |
| Run outcome policy: turn recovery, `Paused`, mid-run compaction, checkpoint, admission validation, tool-argument leniency, approval deadline | `run-reliability.md` |
| Who settles a held approval: Jev, `reviewer_model`, or the human; delegate grants, the delegate clock, the session switch | shipped (ENG-862, protocol 28); as built in [`../design/tools.md`](../design/tools.md) § Approval Policy and [ADR-0041](../adr/0041-jev-delegated-approval.md); receipt [`progress/delegated-approval.md`](./progress/delegated-approval.md) |
| Reference audit of Codex, OpenCode, Pi, and fx; findings F01–F28 | [`../design/harness-scale-audit-2026-09-16.md`](../design/harness-scale-audit-2026-09-16.md) (research, not a plan; F03–F28 unowned) |
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

Optional Jev stacked repairs: [`jev-opt-in.md`](jev-opt-in.md);
[ledger](progress/jev-opt-in.md).
