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
| [`token-efficiency.md`](token-efficiency.md) | TE0–TE8: task-tree economics, tools, selective discovery, evidence reuse and context experiments; reuses existing D6b/T13/cache owners |
| [`speed-first-extensible-agent-harness.md`](./speed-first-extensible-agent-harness.md) | Backend plan, collapsed to what is open: two quiet-host recordings, seven H22 deferrals, Phases 7–9 gated. Shipped design lives in `architecture.md` § Extension Contract and § Performance Discipline |
| [`guide-expansion.md`](./guide-expansion.md) | Correct the shipped guide (GE0), add concepts and workflows pages for newcomers, and write the eight pages the docs site dropped; each guarded by docs-truth. Successor to onboarding UX (closed 2026-09-25; receipt in `progress/onboarding-ux.md`) |
| [`autonomous-core.md`](./autonomous-core.md) | A core that runs one task unattended for 8+ hours and embeds in under 100 lines: progress track first (AP0–AP5: stall reports, sub-agents that answer their brief, non-blocking delegation), then soak harness, run-loop state by reset scope, bounds that reset at seams, `ContinueRun`/auto-continue, loop guard, `qq-harness` composition crate, tool features, store write amplification. AP0–AP3a and AC0.1 in review as one stack; AC7–AC9 moved to `goals.md`; ADR-0048–0050 and 0054 Proposed; absorbs mid-run compaction's MRC-4/5 |
| [`goals.md`](./goals.md) | `/goal`: a session goal the runtime pursues across runs, restarts, outages and days until its check passes, its budget runs out, it is blocked, or the user pauses it. Goal driver, `update_goal`, check command, goal budgets through `RunLimits`, waits and backoff, `/goal`, `qq run --goal`, `qq goal`. G0–G5 planned; ADR-0049 revised, Proposed |
| [`run-reliability.md`](./run-reliability.md) | Sessions finish: turn-level recovery and `Paused`, reactive overflow and un-wedged admission, tolerant checkpoint, admission-time slash validation, lenient tool arguments. RR1–RR7 and RR9 shipped, RR8 shipped except its mid-tool-call re-issue item (ENG-870); open: that RR8 remainder, RR10–RR12; from the 2026-09-21 audit |
| [`terminal-bench-readiness.md`](./terminal-bench-readiness.md) | Harness reliability and Terminal-Bench program; R6–R8 open (R6 candidate designs moved to `tool-layer.md`) |
| [`tool-layer.md`](./tool-layer.md) | Slim, safe, token-efficient built-ins. T1–T9 and T12 shipped (v0.1.0, #45, #49, #50); open: T11 `view_image`, T13 ablations, T14 `select_tools` index; T10 `terminal` gated |
| [`supervised-delegation.md`](./supervised-delegation.md) | Continuation, roster, supervised children, audit; D6b open |
| [`multi-surface-clients.md`](./multi-surface-clients.md) | Web, desktop, and mobile clients over many headless servers. W1, W2, S1, S3 shipped; open: S2 enrollment, S4 exposure, W3, then U/D/M |
| [`run-snapshots.md`](./run-snapshots.md) | Proposed: reversible mutating-run state |
| [`lsp-diagnostics.md`](./lsp-diagnostics.md) | Proposed: diagnostics integration |
| [`decision-models.md`](./decision-models.md) | The only decision-model plan (was `jev.md`). DM1–DM9: decision seam in `qq-provider`, `qq-decision` crate, System One and OpenAI adapters, config, calibration, vendor comparison (ADR-0055). JV1–JV13 carried: repairs, context, hold lifecycle, receipts, shadow, pilot, opt-in UX, speed, qualification. DX1–DX6 differentiation experiments. Proposed |
| [`templates/`](./templates/) | Slice header, pre-flight, receipt, PR body; review checklist |
| [`progress/`](./progress/) | One ledger per plan, root ledger, decisions needed, gate evidence |

## Priority

| # | Next slice | Plan | Why now |
| ---: | --- | --- | --- |
| 0 | AP1 sub-agent brief and AP2 pruned-read headers (parallel), then AP3a checkpoint reports as persisted turns and AP3b stall reports and child answers, then AP4 non-blocking delegation | [`autonomous-core.md`](./autonomous-core.md) § Order | Long runs and sub-agents produce activity without output: children answer only at the end or not at all, and delegating parents had a `spawn_agent` call open for 40 % of their wall time (ADR-0054). No protocol bump for AP1–AP3b (AP3a adds one store column); each is visible in the next day of use |
| 0a | AC0 soak harness (AC0.1 in flight), then AC1 run-loop state extraction, then AC2/AC3 (bounds reset at seams; no single-shot fatal faults); goal branch AC4 + G0 → G2 → G3, and prompt-continuation branch AC5/AC6 | [`autonomous-core.md`](./autonomous-core.md), then [`goals.md`](./goals.md) | Three per-run bounds (4 MiB context reservation, 16 MiB model text, one empty-output retry) end any multi-hour run regardless of model behaviour. Goals recover with fresh runs; prompt-only `ContinueRun` is independent, not a G2 prerequisite. The embedding track (AC10–AC13) runs in parallel |
| 0b | RR8 mid-tool-call re-issue (ENG-870), RR10 lenient arguments, RR11 read-hash ledger | [`run-reliability.md`](./run-reliability.md) | RR1–RR7 and RR9 shipped and RR8 mostly shipped; these are the remaining turn-costing failures |
| 1 | GE0 guide corrections, then GE10 concepts, GE5 keybindings, GE4 environment, then GE2, GE3, GE1, GE9 | [`guide-expansion.md`](./guide-expansion.md) | The site is live and states five false things today; after that come the definitions and references newcomers look for and cannot find. Small, independent, and each extends the docs-truth net |
| 2 | Live qualification of the context-usability stack | — (one manual run; record in `progress/root.md`) | C1–C6 shipped (#56–#64) on fixtures only. Confirm on a real long session: `cache_read_input_tokens > 0` on turn 2 of an Anthropic/Bedrock session, and a ~700 KB transcript on a 200k model sends and compacts |
| 3 | Harness-audit findings F04–F28 triage | [`../design/harness-scale-audit-2026-09-16.md`](../design/harness-scale-audit-2026-09-16.md) § Proposed work order | F01–F03 and F14 shipped (F03 as ADR-0039). F19 (build profiles) and F20 (retention) are now inputs to `autonomous-core.md` AC11 and AC16 |
| 4 | T13 ablation harness (A0–A4s arms) | `tool-layer.md` | Every tool-layer target (≥25 % fewer calls, ≥35 % fewer tokens) is unmeasured until this runs; it also feeds R6's evidence gate for T10 and H10 |
| 5 | D6b paired evaluation (paid runs) and the default decisions it feeds | `supervised-delegation.md` | Decides delegation depth and worker-model defaults with evidence; audit default flipped to `off` in C3 pending B1 |
| 6 | Multi-surface S2 enrollment (ADR-0015) and S4 exposure (ADR-0016); then W3 | `multi-surface-clients.md` | W1/W2/S1/S3 shipped; a remote client is blocked on authentication |
| 7 | T14 `select_tools` index; T11 `view_image` | `tool-layer.md` | Small; T11 needs the provider image content block |
| 8 | Phase 7 — H10 process sandbox | `speed-first-…` | Gated on R6 (T13 evidence, T10 decision) and a platform threat model |
| 9 | Phase 8 — H11 product adapters; Phase 9 — H12 qualification | `speed-first-…` | H11 needs a real consumer; H12 closes the story |
| — | Quiet-host recordings: Phase 5a H0 tail comparison; H20 eight-stream p95 then the 50→20 ms budget | `speed-first-…` | Implemented; tails not repeatable on the shared host; retained, not waived |
| — | Seven H22 deferrals (`StaticHttpAuth`, headless writer, config parse-once, reviewer via `PlanCache`, run-loop enums, args-parse-once, `Arc` calls) | `speed-first-…` § Bundled Fixes | Each is its own slice when that code is next opened |
| — | Run snapshots, LSP diagnostics | proposed plans | No scheduled slice |
| — | JV1 remainder, JV4 and DM1 (parallel), then DM2–DM4, JV5–JV6 | [`decision-models.md`](./decision-models.md) | Opted-in users are prompted for most held calls, and a second vendor (OpenAI Decisions) needs one decision seam instead of a fourth copy of the Jev code. Awaiting plan acceptance; ADR-0055 is accepted |
| — | TE1 offline efficiency baseline, then TE2/TE4 | [`token-efficiency.md`](token-efficiency.md) | Measurement first; paid runs remain with ENG-809 and existing reliability priorities are unchanged |

## Ownership

| Concern | Owner |
| --- | --- |
| Compiled plan, protocol contract, extension lanes, store/provider hot path, perf gates and budgets, headless-contract sequencing (HC1–HC4) | `speed-first-extensible-agent-harness.md` |
| Tool-contract ablations, terminal, sub-agent economics, Terminal-Bench evaluation program, remaining warm-path candidates | `terminal-bench-readiness.md` |
| Built-in tool contracts, output bounding and spill, shell classification, `exec`/`fetch`/`ask_user`/`terminal`, `@` mentions | `tool-layer.md` |
| Continuation on truncation, delegation roster, supervised write children, final-answer audit, paired evaluation | `supervised-delegation.md` |
| Web, desktop, mobile clients; remote server readiness (identity, enrollment, CORS, TLS, workspace catalog) | `multi-surface-clients.md` |
| Reversible mutating-run state | `run-snapshots.md` |
| Long unattended runs: run-loop state and reset scopes, run continuation and auto-continue, loop guard, soak/resource evidence; the embedding surface and `qq-harness` composition crate | `autonomous-core.md` |
| Goals (`/goal`): the session goal, goal driver, `update_goal`, completion check, goal budgets, waits, `qq run --goal` | `goals.md` |
| Mid-run compaction | shipped (ADR-0039); remaining surfaces and live evidence are `autonomous-core.md` AC14 and Goal 5 |
| Diagnostics integration | `lsp-diagnostics.md` |
| Decision models (Jev, OpenAI Decisions, future vendors): the provider seam, `qq-decision`, review, routing, approval delegate, observer, calibration and qualification | [`decision-models.md`](./decision-models.md); design [`../design/decision-models.md`](../design/decision-models.md) |
| First-run and configuration UX, install paths, community files | shipped (onboarding UX, ENG-875); receipt in `progress/onboarding-ux.md` |
| User guide pages and the docs site's content | `guide-expansion.md` |
| Run outcome policy: turn recovery, `Paused`, checkpoint, admission validation, tool-argument leniency, approval deadline | `run-reliability.md` |
| Who settles a held approval: Jev, `reviewer_model`, or the human; delegate grants, the delegate clock, the session switch | shipped (ENG-862, protocol 28); as built in [`../design/tools.md`](../design/tools.md) § Approval Policy and [ADR-0041](../adr/0041-jev-delegated-approval.md); receipt [`progress/delegated-approval.md`](./progress/delegated-approval.md) |
| Reference audit of Codex, OpenCode, Pi, and fx; findings F01–F28 | [`../design/harness-scale-audit-2026-09-16.md`](../design/harness-scale-audit-2026-09-16.md) (research, not a plan; F03–F28 unowned) |
| Shared files, dependency and toolchain bumps, ADR numbering | [`progress/root.md`](./progress/root.md) |
| Cross-cutting efficiency accounting and new evidence/context experiments; D6b/T13/cache retain their owners | [`token-efficiency.md`](token-efficiency.md) |

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
