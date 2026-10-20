# Playbook: slice

A slice is one PR-sized unit from a plan in `docs/plans/`. Follow the
playbook that fits the work (feature, bug fix, refactoring, perf).

1. Read the slice's plan section and the design docs it names.
2. Create the worktree and branch:
   `git worktree add .worktrees/<id> -b <type>/<eng-id>-<id>-<short> origin/main`.
3. Run the wrapped playbook's steps.
4. Run `.qq/skills/qq-mode/playbooks/opening-a-pr.md`. Put the evidence in
   the PR body, not in a ledger file.
