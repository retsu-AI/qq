---
name: qq-mode
description: Start here for any non-trivial change in the qq repo (feature, bug fix, refactor, perf, test cleanup, review, PR). Picks the playbook, sets the todo list, and names the gates that prove the work.
---

# qq mode

The operating procedure for agents in this repository. `AGENTS.md` holds the
repository facts and rules; this skill holds the procedure. When the two
disagree on process, this skill wins; on Rust style or architecture,
`AGENTS.md` wins.

## Non-negotiables

1. **Pick a playbook first.** Match the task to one row below, open
   `playbooks/<file>` next to this file, and copy its numbered steps into your
   todo list verbatim as the first items. A step you skip stays in the list as
   `skip: <reason>`.
2. **Every claim of "works" carries evidence from the real artifact.** A test
   name and its pass, `cargo xtask check` output, a `cargo xtask smoke`
   scenario, or a `verify-qq` drive. "It compiles" and "the change is small"
   are not evidence. Label anything you did not observe as *inferred* or
   *guess* in the same sentence.
3. **Iterate on the narrowest check; finish on the gate.**
   - While editing: `cargo nextest run -p <crate> -E 'test(/<name>/)'`.
   - Before every commit you push: `cargo xtask check`.
   - User-visible behavior: `cargo xtask smoke` (and the matching
     `verify-qq` feature when no scenario covers it).
   - The merge queue runs `cargo xtask check --full`; you do not need to.
4. **Read failures from the report, not the scrollback.**
   `target/qq-check/last.json` lists each failing test, its first panic
   lines, and the exact rerun command. Smoke failures keep their evidence in
   `target/qq-smoke/<scenario>/` (stdout, stderr, `provider-requests.txt`,
   the session store).
5. **Fix the cause, not the gate.** Never weaken an assertion, add a sleep,
   add `#[ignore]`, raise `SLEEP_CEILING`, or add `#[allow(...)]` to get
   green. If a gate is wrong, fix the gate in its own commit and say why.
6. **Encode repeated corrections.** The second time you or a reviewer
   correct the same mistake, add a rule to `cargo xtask lint`
   (`xtask/src/lint.rs`), a type, or a test, and a row to the
   `AGENTS.md` § Rules → enforcers table. Prose is the last resort.

## Playbooks

| Task | Playbook |
| --- | --- |
| A read-only question: how does X work, why, are we sure | `playbooks/investigation.md` |
| A defect to reproduce, root-cause, and fix | `playbooks/bug-fix.md` |
| New or changed behavior | `playbooks/feature.md` |
| Behavior-preserving restructure (rename, extract, move, dedupe) | `playbooks/refactoring.md` |
| A measured slowness or a hot-path change | `playbooks/perf.md` |
| Flaky, slow, duplicated, or low-value tests | `playbooks/test-audit.md` |
| A slice from a plan in `docs/plans/` | `playbooks/slice.md` (wraps one of the above) |
| Drive an open PR to merge-ready | `playbooks/babysit.md` |
| Resume another agent's work or hand yours off | `playbooks/session-pickup.md` |
| A recurring mistake to encode as a rule | `playbooks/correct.md` |
| Any of the above is ready for review | `playbooks/opening-a-pr.md` (every playbook ends here) |

No row fits: write a five-step plan in the same shape (reproduce or
baseline, change, verify on the real surface, encode, PR) and say so.

## Principles

Load the `principles` skill and read the section before you apply it. Name
the principle in your reply when it changed a decision.

- **Prove it works**: before saying done.
- **Fix root causes**: debugging; reproduce first.
- **Test behavior, not implementation**: writing, changing, or keeping a test.
- **Subtract before you add**: sizing a diff; tempted to add a layer.
- **Encode lessons in structure**: correcting the same thing twice.
- **Explain the number**: before trusting a timing or benchmark.
- **Guard the context window**: large outputs, wide searches.
- **Rust style**: writing Rust here (`AGENTS.md` § Rust Style is the rule).

## Autonomy

- Proceed without asking on reversible work: edits, tests, local commits,
  branches, worktrees, Linear comments, opening a draft PR.
- **Always stop and ask before:** force-pushing a shared branch, tagging or
  releasing, deleting user data or sessions, editing another agent's ledger
  or worktree, changing `PROTOCOL_VERSION` or the store schema without a
  plan that names it, or bumping a workspace dependency.
- Reserve ADR numbers in `docs/plans/progress/root.md` before writing one.
- Work in your own worktree when another agent is active
  (`git worktree add .worktrees/<name> -b <branch> origin/main`).

## Subagents

Give a subagent: the playbook file to follow, the owned paths, the exact
gate it must pass (`cargo xtask check`, named smoke scenarios), and what to
return (diff summary plus the `cargo xtask check --evidence` block). Review
its diff yourself; do not forward its summary as your own.

## The reply

Lead with what changed for the user of `qq` and for the next maintainer.
Then: what you verified and how (paste the evidence block), what you did not
verify and why, and open decisions. Short declarative sentences. Link only
artifacts you produced or read.
