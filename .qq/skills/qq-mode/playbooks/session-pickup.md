# Playbook: session pickup and handoff

## Picking up

1. Find the work: the Linear issue, its branch, the plan ledger in
   `docs/plans/progress/`, and any worktree (`git worktree list`).
2. Read the last ledger entry and `git log origin/main..<branch>`.
3. Re-establish ground truth: `cargo xtask check` on the branch head and
   `target/qq-check/last.json`. Trust the report over the previous agent's
   summary.
4. Continue the playbook the previous agent was running from its first
   unfinished step.

## Handing off or pausing

1. Commit work in progress on the branch as `chore(wip): <what>`; squash
   those commits before opening the PR.
2. Append to the ledger: what is done (with evidence), what is in flight,
   the next step, open questions, and the exact command to resume.
3. Leave the worktree in place and name it in the ledger.
