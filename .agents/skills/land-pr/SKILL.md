---
name: land-pr
description: Merge a GitHub PR after proving it is ready and that its head is the reviewed head. Use only when the user asked to merge, land, or ship.
---

# Land a PR

Merge only when the user said merge, land, ship, or "take it all the
way". Green CI is not a reason by itself.

1. **Ready.** Run `.qq/skills/babysit-pr/pr-status.sh <pr>`. It must exit
   0. If not, load `babysit-pr` and come back.
2. **Read the diff yourself.** `gh pr diff <pr>`. Confirm it does only
   what the PR says, adds no secrets or `target/` files, weakens no test,
   and touches nothing the user did not ask for. If anything is off, stop
   and report it.
3. **Pin the head.** Take `head` from step 1. Merge that exact commit so
   nothing pushed after your checks lands:

   ```sh
   gh pr merge <pr> --squash --match-head-commit <head>
   ```

   Do not pass `--admin` and do not skip checks. If GitHub refuses, report
   the reason.
4. **Confirm.** `gh pr view <pr> --json state,mergedAt,mergeCommit`. The
   state must be `MERGED`. A queued or auto-merge request is not a merge.
5. **Clean up.** GitHub deletes the remote branch. Remove your local
   worktree with `git worktree remove .worktrees/<short>` and delete the
   local branch. Never remove a worktree you did not create.

For a stack of PRs, land the lowest one first. Then, on the next branch,
`git merge origin/main`, push, retarget it with
`gh pr edit <next> --base main`, and repeat from step 1 for it. Each PR
needs its own ready result on its own head.

**Reply:** the PR URL, the merge commit, and anything you skipped.
