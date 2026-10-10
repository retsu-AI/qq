# Plans

Active plans only. Shipped plans are deleted with their ledgers; their
receipts live in Git history and their durable contracts in
[`../design/`](../design/). Read
[`../design/architecture.md`](../design/architecture.md) before changing
system boundaries and [`workflow.md`](./workflow.md) before starting a slice.

## Tracker

Open work is tracked in Linear project `qq` (team `ENG`), one milestone per
plan below plus `Harness Audit`, `Evaluation Program`, and `Decisions`.
Ledgers in [`progress/`](./progress/) hold receipts and gate evidence; when
they disagree with Linear on status, Linear is current. Reserve ADR numbers
in [`progress/root.md`](./progress/root.md).

## How to use this directory

| If you are… | Read, in order |
| --- | --- |
| deciding what to work on | this file § Priority, then the plan's status block |
| picking up a slice | [`workflow.md`](./workflow.md), the plan's task index and phase section, [`templates/slice.md`](./templates/slice.md), the plan's ledger in [`progress/`](./progress/) |
| reviewing a PR | [`templates/review-checklist.md`](./templates/review-checklist.md), the slice section, the ledger receipt |
| checking what is in flight | [`progress/README.md`](./progress/README.md) and the plan's ledger |
| recording a decision | [`../adr/README.md`](../adr/README.md); reserve the number in [`progress/root.md`](./progress/root.md) |

## Files

Each plan's ledger has the same name under [`progress/`](./progress/).

| Plan | Scope and what is open |
| --- | --- |
| [`autonomous-core.md`](./autonomous-core.md) | A core that runs one task unattended for 8+ hours and embeds in under 100 lines. AP0–AP4.2, AC0.1, AC10, AC11 and AC12.1 shipped; AP5 evidence window 2026-10-06–13; then AC1 run-loop state, AC2/AC3 bounds that reset at seams, AC5/AC6 continuation, AC12.2–AC16. ADR-0048, 0050 and 0054 Proposed |
| [`goals.md`](./goals.md) | `/goal`: a session goal the runtime pursues across runs, restarts and days until its check passes, its budget runs out, it is blocked, or the user pauses it. G0–G5 planned (with autonomous-core AC4); ADR-0049 Proposed |
| [`compaction.md`](./compaction.md) | Fast, exact, cache-aligned compaction (ADR-0056). CX0–CX4 shipped; CX5 live qualification open |
| [`run-reliability.md`](./run-reliability.md) | Sessions finish: turn recovery and `Paused`, reactive overflow, tolerant checkpoint, admission validation, lenient tool arguments. RR1–RR9 shipped except RR8's mid-tool-call re-issue (ENG-870); open: that item and RR10–RR12 |
| [`tool-layer.md`](./tool-layer.md) | Slim, safe, token-efficient built-ins. T1–T9, T12 and T15–T17 shipped; open: T13 ablations, T14 `select_tools` index, T11 `view_image`; T10 `terminal` gated |
| [`token-efficiency.md`](./token-efficiency.md) | More verified work per token: task-tree economics, tools, selective discovery, evidence reuse and context experiments. TE0 and TE1.1 shipped; the rest of TE1 and TE2–TE8 planned |
| [`guide-expansion.md`](./guide-expansion.md) | The user guide's missing pages, each guarded by docs-truth. GE0 and GE10 shipped; GE1–GE9 planned |
| [`tui-redesign.md`](./tui-redesign.md) | Responsive layout and a legible transcript. U0–U7, U9 and L1–L3 shipped; open: L4 split transcripts, U8 chrome |
| [`jev.md`](./jev.md) | The only Jev plan. JV0, JV1's activation fix and JV2 shipped; JV3–JV13 (precision, context, hold lifecycle, receipts, shadow and pilot, UX, speed, qualification) planned. Proposed |
| [`multi-surface-clients.md`](./multi-surface-clients.md) | Web, desktop and mobile clients over many headless servers. W1, W2, S1 and S3 shipped; open: S2 enrollment, S4 exposure, W3, then U/D/M |
| [`supervised-delegation.md`](./supervised-delegation.md) | Continuation, roster, supervised children, audit. D1–D6a shipped; D6b paired evaluation open |
| [`terminal-bench.md`](./terminal-bench.md) | Harness reliability and the Terminal-Bench evaluation program. Phases 1–5 shipped; open: R6-terminal evidence, R7, R8, `TB-pilot` |
| [`speed-first.md`](./speed-first.md) | The speed-first backend. Phases 0–6 closed; open: two quiet-host recordings and seven H22 deferrals; Phases 7–9 gated. Shipped design is in `architecture.md` § Extension Contract and § Performance Discipline |
| [`run-snapshots.md`](./run-snapshots.md) | Proposed: reversible mutating-run state. No ledger until a slice starts |
| [`lsp-diagnostics.md`](./lsp-diagnostics.md) | Proposed: diagnostics integration. No ledger until a slice starts |

| Process file | Purpose |
| --- | --- |
| [`workflow.md`](./workflow.md) | Slice protocol, ledger rules, review, escalation, dispatch skeletons |
| [`templates/`](./templates/) | Slice header, pre-flight, receipt, PR body; review checklist |
| [`progress/`](./progress/) | One ledger per plan, root ledger, decisions needed, gate evidence |

## Priority

| # | Next slice | Plan | Why now |
| ---: | --- | --- | --- |
| 0 | AP5 seven-day normal-use evidence (2026-10-06–13) | [`autonomous-core.md`](./autonomous-core.md) § Order | Confirms that long runs and sub-agents now produce output, not only activity (ADR-0054) |
| 0a | AC1 run-loop state extraction, then AC2/AC3 (bounds reset at seams; no single-shot fatal faults); goal branch AC4 + G0 → G2 → G3; prompt-continuation branch AC5/AC6 | [`autonomous-core.md`](./autonomous-core.md), then [`goals.md`](./goals.md) | Three per-run bounds (4 MiB context reservation, 16 MiB model text, one empty-output retry) end any multi-hour run regardless of model behaviour. The embedding track (AC12.2–AC13) runs in parallel |
| 0b | CX5 seven-day live qualification | [`compaction.md`](./compaction.md) | CX0–CX4 shipped; the speed and cache-read targets are unconfirmed on real sessions |
| 0c | RR8 mid-tool-call re-issue (ENG-870), RR10 lenient arguments, RR11 read-hash ledger | [`run-reliability.md`](./run-reliability.md) | The remaining turn-costing failures |
| 1 | GE5 keybindings, GE4 environment, then GE2, GE3, GE1, GE9 | [`guide-expansion.md`](./guide-expansion.md) | The references newcomers look for and cannot find. Small, independent, and each extends the docs-truth net |
| 2 | Live qualification of the context-usability stack | — (one manual run; record in `progress/root.md`) | C1–C6 shipped (#56–#64) on fixtures only. Confirm on a real long session: `cache_read_input_tokens > 0` on turn 2 of an Anthropic/Bedrock session, and a ~700 KB transcript on a 200k model sends and compacts |
| 3 | Harness-comparison findings triage | [`../research/harness-comparison.md`](../research/harness-comparison.md) § Proposed work order | F01–F07, F10, F11, F14 and F23–F25 shipped (see `progress/root.md` and Git history). F19 (build profiles) and F20 (retention) are inputs to `autonomous-core.md` AC11 and AC16; the rest are unowned |
| 4 | T13 ablation harness (A0–A4s arms) | [`tool-layer.md`](./tool-layer.md) | Every tool-layer target (≥25 % fewer calls, ≥35 % fewer tokens) is unmeasured until this runs; it also feeds R6's evidence gate for T10 and H10 |
| 5 | D6b paired evaluation (paid runs) and the default decisions it feeds | [`supervised-delegation.md`](./supervised-delegation.md) | Decides delegation depth and worker-model defaults with evidence |
| 6 | Multi-surface S2 enrollment (ADR-0015) and S4 exposure (ADR-0016, reserved); then W3 | [`multi-surface-clients.md`](./multi-surface-clients.md) | A remote client is blocked on authentication |
| 7 | T14 `select_tools` index; T11 `view_image` | [`tool-layer.md`](./tool-layer.md) | Small; T11 needs the provider image content block |
| 8 | L4 split transcripts, then U8 chrome | [`tui-redesign.md`](./tui-redesign.md) | Makes concurrent agents visible side by side at the Ultra tier |
| 9 | Phase 7 — H10 process sandbox; Phase 8 — H11 product adapters; Phase 9 — H12 qualification | [`speed-first.md`](./speed-first.md) | Gated on R6 (T13 evidence, T10 decision), a platform threat model, and a real consumer |
| — | Quiet-host recordings: Phase 5a H0 tail comparison; H20 eight-stream p95 then the 50→20 ms budget | [`speed-first.md`](./speed-first.md) | Implemented; tails not repeatable on the shared host; retained, not waived |
| — | Seven H22 deferrals | [`speed-first.md`](./speed-first.md) § Bundled Fixes | Each is its own slice when that code is next opened |
| — | JV3 precision, then JV4–JV6 | [`jev.md`](./jev.md) | Opted-in users are prompted for most held calls; the design doc's remaining findings are defects. Awaiting plan acceptance |
| — | The rest of TE1, then TE2/TE4 | [`token-efficiency.md`](./token-efficiency.md) | Measurement first; paid runs remain with ENG-809 |
| — | Run snapshots, LSP diagnostics | proposed plans | No scheduled slice |

## Ownership

| Concern | Owner |
| --- | --- |
| Compiled plan, protocol contract, extension lanes, store/provider hot path, perf gates and budgets | [`speed-first.md`](./speed-first.md) |
| Tool-contract ablations, terminal, sub-agent economics, Terminal-Bench evaluation program, remaining warm-path candidates | [`terminal-bench.md`](./terminal-bench.md) |
| Built-in tool contracts, output bounding and spill, shell classification, `exec`/`fetch`/`ask_user`/`terminal`, `@` mentions | [`tool-layer.md`](./tool-layer.md) |
| Continuation on truncation, delegation roster, supervised write children, final-answer audit, paired evaluation | [`supervised-delegation.md`](./supervised-delegation.md) |
| Web, desktop, mobile clients; remote server readiness (identity, enrollment, CORS, TLS, workspace catalog) | [`multi-surface-clients.md`](./multi-surface-clients.md) |
| Long unattended runs: run-loop state and reset scopes, run continuation and auto-continue, loop guard, soak/resource evidence; the embedding surface and `qq-harness` | [`autonomous-core.md`](./autonomous-core.md) |
| Goals (`/goal`): the session goal, goal driver, `update_goal`, completion check, goal budgets, waits, `qq run --goal` | [`goals.md`](./goals.md) |
| Compaction speed, fidelity and cache alignment | [`compaction.md`](./compaction.md) |
| Run outcome policy: turn recovery, `Paused`, checkpoint, admission validation, tool-argument leniency | [`run-reliability.md`](./run-reliability.md) |
| TUI layout, transcript rendering, chrome | [`tui-redesign.md`](./tui-redesign.md) |
| Jev review, routing, approval delegate, observer; their qualification | [`jev.md`](./jev.md) |
| User guide pages and the docs site's content | [`guide-expansion.md`](./guide-expansion.md) |
| Cross-cutting efficiency accounting and new evidence/context experiments; D6b/T13/cache retain their owners | [`token-efficiency.md`](./token-efficiency.md) |
| Reversible mutating-run state | [`run-snapshots.md`](./run-snapshots.md) |
| Diagnostics integration | [`lsp-diagnostics.md`](./lsp-diagnostics.md) |
| Shared files, dependency and toolchain bumps, ADR numbering | [`progress/root.md`](./progress/root.md) |

Shipped, and owned by `design/` alone: side questions
([`side-questions.md`](../design/side-questions.md)), delegated approval
([`tools.md` § Approval Policy](../design/tools.md#approval-policy),
ADR-0041), MCP tool-set pinning (ADR-0046), Codex device authorization,
first-run and onboarding UX, and in-run compaction (ADR-0039).

## Conventions

- Slice IDs come from the plan's task index (`H20`, `HC3`, `R6-search`,
  `D6b`); split large tasks in the ledger as `H20.1`, `H20.2`.
- A plan and its ledger share one file name.
- A plan's status block is derived from its ledger and updated in the same PR
  that ships the work, or the immediately following docs PR.
- Record a pre-change baseline for every named performance gate before the
  change lands ([`../runbooks/perf-recording.md`](../runbooks/perf-recording.md)).
- Phase sections follow one template: status, tasks, acceptance, receipt of at
  most about fifteen lines with commit SHAs. When a phase closes, collapse its
  section to one row in the plan's completed-phases table and write the gate
  file in `progress/`.
- Research that motivated a plan belongs in [`../research/`](../research/),
  not in the plan.
- When a plan is fully shipped, move any durable contract into `../design/`,
  delete the plan and its ledger, and update this index.
