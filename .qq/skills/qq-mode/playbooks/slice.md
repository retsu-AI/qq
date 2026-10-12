# Playbook: slice

A slice is one PR-sized unit from a plan in `docs/plans/`. This playbook
wraps the one that fits the work (feature, bug fix, refactoring, perf); the
process rules are in `docs/plans/workflow.md`.

1. Read `docs/plans/workflow.md` §2, the slice's plan section, and the
   design docs it names.
2. Confirm its inputs are merged (`git log origin/main --oneline | rg <id>`)
   and no ledger row in `docs/plans/progress/` is `In progress` on the same
   paths.
3. Create the worktree and branch:
   `git worktree add .worktrees/<id> -b <type>/<eng-id>-<id>-<short> origin/main`.
4. Record the pre-change baseline for every gate the slice names.
5. Set the ledger row to `In progress` with branch, date, baseline path.
6. Run the wrapped playbook's steps.
7. Append the ledger receipt (≤ 15 lines): commits, tests added, gate
   numbers before/after, deviations.
8. A slice touching `sessions/`, `store/`, approvals, provider retry, or the
   plan cache needs an independent review by a second agent (workflow §4).
9. Run `playbooks/opening-a-pr.md` with the body from
   `docs/plans/templates/slice.md`.
