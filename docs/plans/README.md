# Plans: remaining work

Product direction is defined once in [product](../design/product.md): fast,
responsive useful work and a developer-friendly local experience, with safety,
durability and bounded resources as constraints. Plans specify unfinished
outcomes and acceptance, not a second description of shipped behavior.

## Work order

1. **Finish and qualify the core loop.** [Reliability](reliability.md) owns
   recovery, useful progress, compaction and their outstanding live/resource
   gates. Shipped instrumentation is not a passing qualification result.
2. **Make it easy to use and change.** [Developer experience](developer-experience.md)
   owns missing guide references, contributor feedback and remaining test audits.
   This can run alongside runtime work on disjoint paths.
3. **Deliver bounded durable goals.** [Goals](goals.md): G0/AC4 → G2 → G3 →
   G4 → G5 have shipped; ADR-0049 acceptance and resource/performance
   qualification remain. An implementation checkpoint is not whole-plan delivery.
4. **Optimize from evidence.** [Terminal-Bench](terminal-bench.md) owns
   paid tool/delegation evaluations and their default decisions;
   [token efficiency](token-efficiency.md) owns cross-cutting accounting and
   experiments without recreating those evaluations. Paid runs need ENG-809
   approval and a spend cap. [Performance receipts](progress/performance-gates.md)
   retain failed/unqualified H0 results; release acceptance is not a pass.
5. **Expand only behind demonstrated need.** TUI, Jev and remote clients
   keep their specific gates below. Sandbox/compatibility adapters require
   a threat model/real consumer, not speculative framework work.

This is ordering by outcome, not a command to serialize independent work.
A changed priority must name the acceptance it unlocks. No calendar date,
merged PR or closed tracker issue substitutes for a missing measurement.

## Owners and boundaries

| Plan | Remaining scope |
| --- | --- |
| [reliability](reliability.md) | AC/AP recovery and embedding residuals, AP5 productivity, AB6 action bias, CX5 compaction, integrated seven-day reliability |
| [developer experience](developer-experience.md) | GE1–GE9 guide work; AF5 tracing, remaining AF6 audits, AF9 lessons, AF10 evaluation; merge-queue operator follow-up |
| [goals](goals.md) | Durable goal/loop-guard/check-authority specification; G0/G2–G5 shipped; ADR-0049 acceptance and resource/performance qualification remain |
| [tool layer](tool-layer.md) | T13 tool ablations, T14 selective index, T11 images; T10 terminal only on trajectory evidence |
| [Terminal-Bench](terminal-bench.md) | R6 tool experiment method, R7/D6b delegation economics/defaults, remaining R8, TB-pilot and benchmark integrity |
| [token efficiency](token-efficiency.md) | Remaining TE1 accounting and TE2–TE8 experiments; consumes D6b/T13/cache receipts |
| [speed first](speed-first.md) | Residual backend performance gates, H22 deferrals and gated H10–H12; shipped backend design is in architecture |
| [TUI](tui-redesign.md) | L4 split transcripts, U8 chrome |
| [multi-surface clients](multi-surface-clients.md) | Enrollment/exposure before remote clients; remaining W/S/U/D/M work |
| [run snapshots](run-snapshots.md) | Unscheduled proposal; no active delivery inferred |
| [v0.1.8 preparation](release-0.1.8.md) | Pre-bump gates, contract deltas and the draft Upgrading block for the v0.1.8 bump PR |
| [LSP diagnostics](lsp-diagnostics.md) | Unscheduled proposal; no active delivery inferred |
| [auth providers](auth-providers.md) | AP0–AP4 behaviour-preserving qq-auth refactor; AP4b perf fixtures; AP5–AP8 provider-neutral credentials, configuration and OAuth grants; API-key presets and permitted OAuth grants |

Linear project `qq`, team `ENG`, tracks live assignments and issue status.
Query it when starting work; this cleanup does not close issues or assert
remote PR state. [Progress](progress/README.md) records local receipts and
[workflow](workflow.md) defines slice/review procedure. Reserve ADR numbers
in [root](progress/root.md); do not rewrite accepted ADRs during cleanup.

## Consolidation and lifecycle

- `autonomous-core`, `action-bias`, `compaction` → **reliability**.
- `agent-feedback`, `guide-expansion` → **developer experience**.
- `supervised-delegation` → **Terminal-Bench R7/D6b**; continuation and
  supervised-write contracts already live in design.
- v0.1.7 publication was verified on 2026-10-18 (receipt in
  [root](progress/root.md)); its temporary plan and ledger are retired.
  v0.1.8 preparation lives in [release-0.1.8](release-0.1.8.md). The release
  procedure remains [the runbook](../runbooks/release.md).


Existing slice IDs and issues survive consolidation. Old ledgers are marked
historical and retained where ADRs/research depend on their evidence; new
receipts go to the successor. Recover deleted plan detail with
`git log -- docs/plans/<name>.md`, not a parallel archive of stale plans.

A plan owns future acceptance; design owns shipped contracts; research is a
dated snapshot; ledgers own evidence. Remove completed implementation detail
rather than maintaining duplicate status tables. On completion, transfer any
remaining contract to design, delete the plan, and remove navigation links.
Historical ledgers may remain only when named evidence consumers need them,
clearly marked with their successor and no active assignment implied.
