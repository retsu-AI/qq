# QQ documentation

| If you want to know… | Read |
| --- | --- |
| how the system works today | [`design/architecture.md`](design/architecture.md), then the topic docs below |
| why something is the way it is | [`adr/README.md`](adr/README.md) |
| what is being built next and how it is accepted | [`plans/README.md`](plans/README.md) |
| what is in flight right now and its evidence | [`plans/progress/README.md`](plans/progress/README.md) |
| how to do a unit of work or review one | [`plans/workflow.md`](plans/workflow.md) |
| how to run, measure, or qualify something | `runbooks/` below |

## Design — present tense, the system as built

- [`architecture.md`](design/architecture.md) — system shape, crate layout,
  runtime, compiled plans, persistence, hosting boundary, deferred items.
- [`product.md`](design/product.md) — product intent, priorities, scope.
- [`protocol.md`](design/protocol.md) — HTTP/SSE wire protocol and route
  contract.
- [`headless-contract.md`](design/headless-contract.md) — `qq run` JSONL/exit
  contract and the supervisor boundary.
- [`providers.md`](design/providers.md) — provider validation standard.
- [`tools.md`](design/tools.md) — tool loop, containment, approvals, shell,
  MCP.
- [`transcript.md`](design/transcript.md), [`theme.md`](design/theme.md) —
  TUI rendering.
- [`harness-audit-2026-08.md`](design/harness-audit-2026-08.md) — reference
  audit of Codex, OpenCode, Pi, fx, and the Hermes boundary (research; does
  not change as work ships).

## Decisions — `adr/`

Accepted decisions with context, alternatives, consequences, and code
anchors. Immutable once accepted; superseded by a new ADR. Index and "when to
write one" in [`adr/README.md`](adr/README.md).

## Plans — `plans/`

Active plans only, each with a status block, task index, and acceptance.
Priority order and ownership in [`plans/README.md`](plans/README.md). Process
in [`plans/workflow.md`](plans/workflow.md). Templates in
[`plans/templates/`](plans/templates/).

## Progress — `plans/progress/`

One ledger per plan, a root ledger for shared-file work and ADR numbering,
[`decisions-needed.md`](plans/progress/decisions-needed.md), and phase-gate
evidence.

## Runbooks — `runbooks/`

- [`local-dev.md`](runbooks/local-dev.md) — toolchain, gates, test
  environment, worktrees.
- [`perf-recording.md`](runbooks/perf-recording.md) — baseline/candidate
  procedure, focused fixtures, same-binary control, host conditions.
- [`windows-ci.md`](runbooks/windows-ci.md) — the targeted Windows job and how
  to extend it.

## Conventions

1. Design docs are stateless: no status lines, checklists, or pending markers.
   Amend them in the commit that changes behavior.
2. ADRs are one decision, one page, immutable once accepted.
3. Plans are mortal: phases collapse to one row when they close; a fully
   shipped plan moves its durable content to `design/` and is deleted.
4. Ledgers are append-only evidence with one writer each; raw measurements
   stay under `target/qq-perf/` and out of Git.
5. Research that motivated a plan lives in `design/`, not in the plan.
