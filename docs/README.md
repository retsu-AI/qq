# QQ documentation

**Using QQ?** Start with the [user guide](guide/README.md): install,
quickstart, configuration and CLI references, troubleshooting. Everything
below is for people changing QQ.

| If you want to know… | Read |
| --- | --- |
| how the system works today | [`design/architecture.md`](design/architecture.md), then the topic docs below |
| why something is the way it is | [`adr/README.md`](adr/README.md) |
| what evidence motivated a plan | `research/` below |
| what is being built next and how it is accepted | [`plans/README.md`](plans/README.md) |
| how to do a unit of work or review one | [`AGENTS.md`](../AGENTS.md), [`CONTRIBUTING.md`](../CONTRIBUTING.md) |
| how to run, measure, or qualify something | `runbooks/` below |

## Design — `design/`, the system as built

- [`architecture.md`](design/architecture.md) — system shape, crate layout,
  runtime, compiled plans, persistence, hosting boundary, deferred items.
- [`product.md`](design/product.md) — product intent, priorities, scope.
- [`protocol.md`](design/protocol.md) — HTTP/SSE wire protocol, route
  contract, and version history.
- [`headless-contract.md`](design/headless-contract.md) — `qq run` JSONL/exit
  contract and the supervisor boundary.
- [`providers.md`](design/providers.md) — provider validation standard.
- [`tools.md`](design/tools.md) — tool loop, built-in tools and their
  bounding/spill boundary, containment, edit semantics, shell classification,
  `@` mentions, approvals (including
  [delegated approval](design/tools.md#approval-policy)), MCP and embedded
  hosts.
- [`side-questions.md`](design/side-questions.md) — `/btw` side questions:
  captured context, least-authority tools, bounds, durability.
- [`layout.md`](design/layout.md), [`transcript.md`](design/transcript.md),
  [`theme.md`](design/theme.md) — TUI layout tiers and panes, transcript
  rendering, themes.

## Research — `research/`, dated evidence behind plans

Snapshots of QQ and reference harnesses (Codex, OpenCode, Pi, fx) at a
recorded commit. They are not updated as work ships; each names its snapshot
date and the plan that acts on it.

- [`harness-comparison.md`](research/harness-comparison.md) (2026-09-16) —
  reliability findings F01–F28, comparative capability matrix, core versus
  adapter placement, acceptance criteria.
- [`run-reliability.md`](research/run-reliability.md) (2026-09-21) — why
  sessions do not finish: failure taxonomy from the live session store,
  findings R01–R12. Current work: [core stabilization](plans/README.md).
- [`onboarding.md`](research/onboarding.md) (2026-09-22) — first-run and
  configuration UX, findings O01–O20. The onboarding plan that acted on it
  has closed.
- [`token-efficiency.md`](research/token-efficiency.md) (2026-09-23) —
  baseline mechanisms and hypotheses. Current work: [core stabilization](plans/README.md).
- [`core-autonomy.md`](research/core-autonomy.md) (2026-09-28) — what stops
  `qq-core` from running one task unattended for hours and from being
  embedded. Current work: [core stabilization](plans/README.md).
- [`goal-pursuit.md`](research/goal-pursuit.md) (2026-09-28) — how the
  reference harnesses pursue one objective over time, and the positions
  QQ's `/goal` takes. Current work: [core stabilization](plans/README.md).
- [`action-bias.md`](research/action-bias.md) (2026-10-13) — why runs read
  instead of finishing: shell reads counted as work, re-reads, delegation
  wall time, and what the reference harnesses do instead. Current work: [core stabilization](plans/README.md).

## Decisions — `adr/`

Accepted decisions with context, alternatives, consequences, and code
anchors. Immutable once accepted; superseded by a new ADR. Index and "when to
write one" in [`adr/README.md`](adr/README.md).

## Plans — `plans/`

One [core stabilization plan](plans/README.md) defines the remaining work
and its acceptance. Issues and PRs carry assignments and verification. Retired
plans and evidence remain recoverable through Git history; no progress ledger
or reservation document is required.

## Runbooks — `runbooks/`

- [`testing.md`](runbooks/testing.md) — the feedback loop (`cargo xtask
  check`, `smoke`, `lint`), reading failures, test policy, nextest, and what
  CI runs on pull requests versus the merge queue.
- [`local-dev.md`](runbooks/local-dev.md) — toolchain, gates, test
  environment, worktrees.
- [`windows-ci.md`](runbooks/windows-ci.md) — the targeted Windows job and how
  to extend it.
- [`perf-recording.md`](runbooks/perf-recording.md) — baseline/candidate
  procedure, focused fixtures, same-binary control, host conditions.
- [`run-output-queries.md`](runbooks/run-output-queries.md) — read-only
  session-store queries: do long runs and sub-agents produce output, or only
  activity.
- [`action-bias-qualification.md`](runbooks/action-bias-qualification.md) —
  bounded snapshot export, pinned classifier recount and cohort provenance.
- [`tui-qa.md`](runbooks/tui-qa.md) — credential-free, loopback-only TUI
  diagnostic fixture and cleanup.
- [`release.md`](runbooks/release.md) — `cargo xtask release`, the tag-driven
  release workflow, targets, and `qq --version`.
- [`website.md`](runbooks/website.md) — how the docs site is generated from
  `guide/`, built, checked, and deployed to GitHub Pages.

## User guide — `guide/`

Task-oriented pages for people using QQ, amended in the same PR as the
behavior they describe: [`guide/README.md`](guide/README.md). Plain
Markdown; the site at <https://retsu-ai.github.io/qq/> is generated from
these files at build time ([`runbooks/website.md`](runbooks/website.md)).

## Conventions

1. Design docs are stateless: no status lines, checklists, or pending markers.
   Amend them in the commit that changes behavior.
2. ADRs are one decision, one page, immutable once accepted.
3. Keep one current stabilization plan. Remove completed work; move a durable
   contract to `design/` only when it describes shipped behavior. Historical
   links can point to the original commit instead of maintaining old ledgers.
4. Keep raw measurements under `target/` and out of Git. Record relevant
   results with the issue or PR.
5. Research is dated evidence, not a work queue or an implementation claim.
6. File names describe their contents, without dates or slice IDs.
