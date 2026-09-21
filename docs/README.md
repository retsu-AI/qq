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
- [`tools.md`](design/tools.md) — tool loop, built-in tools and their
  bounding/spill boundary, containment, edit semantics, shell classification,
  `@` mentions, approvals, MCP and embedded hosts.
- [`layout.md`](design/layout.md), [`transcript.md`](design/transcript.md),
  [`theme.md`](design/theme.md) — TUI layout tiers and panes, transcript
  rendering, themes.
- [`harness-scale-audit-2026-09-16.md`](design/harness-scale-audit-2026-09-16.md)
  — reference audit of Codex, OpenCode, Pi, and fx against QQ: reliability
  findings (F01–F28), comparative capability matrix, core versus adapter
  placement, and acceptance criteria (research; supersedes the August audit
  and September catalog).

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
- [`tui-qa.md`](runbooks/tui-qa.md) — explicit credential-free, loopback-only
  TUI diagnostic fixture and cleanup. The ordinary user-global config path
  may be a leaf symlink; this fixture still requires a regular `config.ron`.
- [`perf-recording.md`](runbooks/perf-recording.md) — baseline/candidate
  procedure, focused fixtures, same-binary control, host conditions.
- [`windows-ci.md`](runbooks/windows-ci.md) — the targeted Windows job and how
  to extend it.
- [`release.md`](runbooks/release.md) — `cargo xtask release`, the tag-driven
  release workflow, targets, and `qq --version`.

## Conventions

1. Design docs are stateless: no status lines, checklists, or pending markers.
   Amend them in the commit that changes behavior.
2. ADRs are one decision, one page, immutable once accepted.
3. Plans are mortal: phases collapse to one row when they close; a fully
   shipped plan moves its durable content to `design/` and is deleted.
4. Ledgers are append-only evidence with one writer each; raw measurements
   stay under `target/qq-perf/` and out of Git.
5. Research that motivated a plan lives in `design/`, not in the plan.

Jev: [review and direction](design/jev-runtime-review-2026-09-18.md),
[opt-in runbook](runbooks/jev.md), [implementation plan](plans/jev-opt-in.md).
