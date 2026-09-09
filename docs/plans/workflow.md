# Workflow: plans, slices, ledgers, review

**Status:** Active from 2026-09-08.
**Audience:** every agent (implementer or reviewer) and the lead.

This is the operating contract for doing work in this repository with one or
several agents at once. If a plan and this document disagree, this document
wins for *process*; the plan wins for *what to build*. `AGENTS.md` wins for
Rust style and repository rules.

## 1. The four kinds of document

| Kind | Where | Tense | Lifecycle |
| --- | --- | --- | --- |
| Design | `docs/design/` | Present: the system as built | Amended in the same commit that changes behavior; no status lines |
| Decision | `docs/adr/` | Past: what was decided and why | Immutable once Accepted; superseded by a new ADR |
| Plan | `docs/plans/*.md` | Future: sequence, gates, acceptance | Mortal; collapses as phases close; deleted when shipped |
| Ledger | `docs/plans/progress/` | Now: what is in flight and its evidence | Append-only status; one writer per file |

Runbooks (`docs/runbooks/`) are operational how-tos and follow design rules:
present tense, amended when the procedure changes.

A plan states *intent and acceptance*. A ledger records *evidence*. An ADR
records *a decision a future agent would otherwise re-litigate*. Design
records *the result*. Do not put status in design, evidence in plans, or
rationale only in ledgers.

## 2. Slices

A **slice** is one PR-sized unit of work: one plan task or a bounded part of
one, one branch, one review. Every slice has an ID from its plan (`H20`,
`HC3`, `R6-2`, `D6b`), a goal, inputs (what must already be merged), owned
paths, acceptance (tests, commands, measurements), and the docs it must
produce.

Slice IDs come from the owning plan's task index. If a task is too large for
one PR, split it in the ledger as `H20.1`, `H20.2` with their own acceptance
subsets; the plan keeps the parent row.

### 2.1 Before starting

1. Read `AGENTS.md`, this document, the plan section for the slice, and the
   design docs the slice touches. Read `docs/design/architecture.md` before
   changing any boundary.
2. Confirm inputs are merged: `git log origin/main --oneline | grep <id>`.
3. Check the ledger for a conflicting `In progress` slice on the same paths.
   If one exists, coordinate through `progress/root.md` rather than starting.
4. Branch from current `origin/main`: `<type>/<linear-id>-<slice>-<short>`
   per `AGENTS.md` (for example `perf/dev-231-h20-control-admission`).
   Use an isolated worktree when another writing agent is active.
5. Record a **pre-change baseline** for every performance gate the slice
   names, following [`../runbooks/perf-recording.md`](../runbooks/perf-recording.md).
   Commit nothing until the baseline is captured.
6. Set the ledger row to `In progress` with branch, date, and baseline
   location.

### 2.2 While working

- Test-first for behavior and regressions: the first commit names the failing
  test from the slice's acceptance list.
- Stay inside owned paths plus the ledger and the docs the slice names. If a
  shared file must change (root `Cargo.toml`, CI, `benchmarks/perf/budgets-*`,
  `docs/design/architecture.md`, `docs/adr/README.md`), file a root request
  in `progress/root.md` and continue with what does not depend on it.
- Commit small with Conventional Commits; scope is the crate or area, and the
  body names the slice ID.
- Run the narrowest useful test while iterating; run the workspace gates
  before every push. Do not push red.
- Write the design amendment and any ADR in the same PR. Undocumented
  behavior is incomplete.
- Append to your ledger at least every working session: shipped, in progress,
  blocked, evidence so far. Three lines is enough; a receipt block when
  something lands.

### 2.3 Finishing

1. Workspace gates green (`AGENTS.md` § Developer Workflow, plus the minimal
   provider profile when `qq-provider` changed).
2. Post-change measurement for every named gate, recorded next to the
   baseline; A/B and same-binary A/A per the perf runbook when the gate is a
   tail.
3. PR body from [`templates/slice.md`](./templates/slice.md): spec link, what
   changed, how verified (exact commands and counts), measurements, docs,
   follow-ups or decisions needed.
4. Ledger row to `In review` with the PR link. Linear issue state current.
5. After merge: `Shipped (<sha>)`. Update the plan's status block and task
   index in the same or an immediately following docs PR. Collapse any phase
   that closed.

### 2.4 Definition of done for a slice

All acceptance items demonstrated by a test or command in the PR; workspace
gates pass; named performance gates measured before and after with the
receipt in the ledger; design docs amended; ADR written when § 4 of
`docs/adr/README.md` applies; plan status and task index current; Linear
issue and PR accurate; no secrets, credentials, `target/`, or generated
evidence committed.

## 3. Ledgers

`docs/plans/progress/` holds one ledger per plan plus `root.md`,
`decisions-needed.md`, and gate evidence files.

Rules:

1. **One writer per ledger.** The agent working a plan's slice writes that
   plan's ledger. Nobody else edits it; ask through `root.md`.
2. **Top table is current state.** Columns: slice, goal, status, branch/PR,
   notes. Status values: `Planned` · `In progress` · `In review` ·
   `Shipped (sha)` · `Blocked (reason)` · `Dropped (reason)`.
3. **Below the table is append-only.** Dated entries, newest last. Receipts
   are ≤ 15 lines: commit, tests added and counts, gate measurements before
   and after, deviations from the plan, open items.
4. **Raw evidence stays out of Git.** Perf reports, pressure snapshots, and
   logs live under `target/qq-perf/<slice>-<date>/`; the ledger records the
   path and the numbers that matter.
5. **The plan's status block is derived from ledgers**, not the other way
   around. When a ledger row becomes `Shipped`, the plan is updated in the same
   PR or the next docs PR.
6. `decisions-needed.md` is append-only and anyone may add a row; only the
   lead resolves. Reference an entry in code as `TODO(decision:<n>)`.
7. Gate files (`g-<name>.md`) record a phase gate run: date, exact `main`
   SHA, commands, counts, pass/fail, and what was not tested.

## 4. Review

Reviews check two axes in order, using
[`templates/review-checklist.md`](./templates/review-checklist.md):

1. **Spec conformance.** Does the PR do what the slice says, no more, no less?
   Missing acceptance, unmeasured named gates, or edits outside owned paths
   are blocking.
2. **Standards conformance.** `AGENTS.md` Rust style, bounded resources, no
   lock across `.await`, no blocking on Tokio workers, typed errors, tests
   that assert behavior, persist-before-publish preserved.

Outcomes: `Approve`, `Request changes` (blocking first, then should-fix, then
nits, each with `file:line`), or `Escalate` when the reviewer believes the
*spec* is wrong. Escalation writes a `decisions-needed.md` row and does not
silently change the code. A slice may merge with a `TODO(decision:<n>)` only
if the lead agrees.

Independent review by a second agent is required for any slice that touches
`sessions/`, `store/`, approval, provider retry, or the plan cache.

## 5. Sync points

- **Slice start**: ledger row `In progress`; baseline captured.
- **Every session**: three-line ledger append.
- **Slice end**: receipt in the ledger; plan status block updated.
- **Phase gate**: the lead runs the gate on `main`, writes
  `progress/g-<phase>.md`, and collapses the phase section in the plan to one
  table row. Failures become slices at the top of the next phase.

## 6. Conflicts

| Situation | Resolution |
| --- | --- |
| Two slices need the same shared file | Root request; whichever lands first owns it; the other rebases |
| A slice discovers a plan error | `decisions-needed.md` row, conservative interpretation, `TODO(decision:<n>)`, continue |
| A tail gate fails on a loaded host | Run the same-binary A/A control; record both; do not waive; schedule a quiet-host run (perf runbook) |
| A dependency bump is needed | Root slice; one bump across the workspace, never inside a feature slice |
| Concurrent edits to one ledger | The second writer moves their entry to `root.md` and asks for reassignment |

## 7. Dispatch skeleton

```text
You are working slice <id> from docs/plans/<plan>.md on branch <branch>
(worktree <path> if applicable). Read AGENTS.md, docs/plans/workflow.md,
the slice section, and the design docs it names. Owned paths: <list>.
Record the pre-change baseline per docs/runbooks/perf-recording.md before
any code change. Work test-first. Append to docs/plans/progress/<plan>.md
every session. Open a PR using docs/plans/templates/slice.md. If blocked,
record the blocker in the ledger and stop.
```

## 8. Review skeleton

```text
Review PR <url> for slice <id> (spec: docs/plans/<plan>.md#<anchor>).
Check out the PR head in an isolated worktree and run the workspace gates.
Use docs/plans/templates/review-checklist.md. Report blocking, should-fix,
nits with file:line. Verdict: Approve | Request changes | Escalate.
```
