---
name: qq-mode
description: Start here for any non-trivial task in the qq repo (feature, bug fix, refactor, perf, tests, protocol change, review, PR). Picks the playbook, sets the todo list, and names the gates that prove the work.
---

# qq mode

The operating procedure for agents in this repository. `AGENTS.md` holds the
repository facts and rules; this skill holds the procedure. When the two
disagree on process, this skill wins; on Rust style or architecture,
`AGENTS.md` wins.

## Non-negotiables

1. **Pick a playbook** that matches the task and follow its steps where they
   help. Skip steps that don't apply; the user's request is the scope.
2. **Every claim of "works" carries evidence from the real artifact.** A test
   name and its pass, `cargo xtask check` output, a `cargo xtask smoke`
   scenario, or a `verify-qq` drive. "It compiles" and "the change is small"
   are not evidence. Label anything you did not observe as *inferred* or
   *guess* in the same sentence.
3. **Iterate on the narrowest check; run the gate once at the end.**
   - While editing: `cargo nextest run -p <crate> -E 'test(/<name>/)'`.
   - Before pushing: `cargo xtask check`, once.
   - User-visible behavior: `cargo xtask smoke`.
   - Perf, release builds, and `--full` belong to CI unless the task is
     specifically about them. Never `sleep` to wait on a command.
4. **Read failures from the report, not the scrollback.**
   `target/qq-check/last.json` lists each failing test, its first panic
   lines, and the exact rerun command. Smoke failures keep their evidence in
   `target/qq-smoke/<scenario>/` (stdout, stderr, `provider-requests.txt`,
   the session store).
5. **Fix the cause, not the gate.** Never weaken an assertion, add a sleep,
   add `#[ignore]`, raise `SLEEP_CEILING`, or add `#[allow(...)]` to get
   green. If a gate is wrong, fix the gate in its own commit and say why.
6. **Encode repeated corrections** in a lint rule or test when the same
   mistake recurs. Prose is the last resort.

## Playbooks

| Task | Playbook |
| --- | --- |
| A read-only question: how does X work, why, are we sure | `.qq/skills/qq-mode/playbooks/investigation.md` |
| A defect to reproduce, root-cause, and fix | `.qq/skills/qq-mode/playbooks/bug-fix.md` |
| New or changed behavior | `.qq/skills/qq-mode/playbooks/feature.md` |
| Wire type, plan descriptor, or store schema change | `.qq/skills/qq-mode/playbooks/feature.md` plus the `protocol-bump` skill (`.qq/packs/protocol/skills/protocol-bump/SKILL.md`) |
| Behavior-preserving restructure (rename, extract, move, dedupe) | `.qq/skills/qq-mode/playbooks/refactoring.md` |
| A measured slowness or a hot-path change | `.qq/skills/qq-mode/playbooks/perf.md` |
| Flaky, slow, duplicated, or low-value tests | `.qq/skills/qq-mode/playbooks/test-audit.md` |
| TUI rendering or layout change | `.qq/skills/qq-mode/playbooks/feature.md`; goldens via `QQ_UPDATE_GOLDENS=1`, then review `git diff crates/qq-tui/tests/goldens/` |
| Docs-only change | `cargo xtask check` (fmt and lint; guide edits also run `docs_truth`), then `.qq/skills/qq-mode/playbooks/opening-a-pr.md` |
| CI, xtask, or skill change | `.qq/skills/qq-mode/playbooks/feature.md`; prove it with `cargo nextest run -p xtask` and one real run of the changed command |
| A slice from a plan in `docs/plans/` | `.qq/skills/qq-mode/playbooks/slice.md` (wraps one of the above) |
| Review someone else's PR | `.qq/skills/qq-mode/playbooks/review.md` |
| Drive an open PR to merge-ready | `.qq/skills/qq-mode/playbooks/babysit.md` |
| Resume another agent's work or hand yours off | `.qq/skills/qq-mode/playbooks/session-pickup.md` |
| A recurring mistake to encode as a rule | `.qq/skills/qq-mode/playbooks/correct.md` |
| Dependency bump or release | stop and ask; then `docs/runbooks/release.md` for releases |
| Any of the above is ready for review | `.qq/skills/qq-mode/playbooks/opening-a-pr.md` (every playbook ends here) |

No row fits: write a five-step plan in the same shape (reproduce or
baseline, change, verify on the real surface, encode, PR) and say so.

## Principles

Load the `principles` skill (`.qq/skills/principles/SKILL.md`) and read
the section before you apply it. Name
the principle in your reply when it changed a decision.

- **Prove it works**: before saying done.
- **Fix root causes**: debugging; reproduce first.
- **Test behavior, not implementation**: writing, changing, or keeping a test.
- **Subtract before you add**: sizing a diff; tempted to add a layer.
- **Encode lessons in structure**: correcting the same thing twice.
- **Explain the number**: before trusting a timing or benchmark.
- **Guard the context window**: large outputs, wide searches.
- **Rust style**: writing Rust here (`AGENTS.md` § Rust style is the rule).

## Autonomy

- Proceed without asking on reversible work: edits, tests, local commits,
  branches, worktrees.
- **Always stop and ask before:** force-pushing a shared branch, tagging or
  releasing, deleting user data or sessions, editing another agent's
  worktree, or bumping a workspace dependency.
- When the user asks "can I merge?", answer yes or no in the first word,
  with the single blocker if no. Budgets and bot emoji are not blockers.
- Work in your own worktree when another agent is active
  (`git worktree add .worktrees/<name> -b <branch> origin/main`). Do not
  share `CARGO_TARGET_DIR` with a worktree whose builds you depend on at the
  same moment; `cargo xtask` reports and homes stay per checkout regardless.

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
