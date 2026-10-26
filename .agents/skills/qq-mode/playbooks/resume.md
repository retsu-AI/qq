# Resume

You own the resume point. Trust the prior work's record, then verify its
claims on the real artifact.

1. Find the work: the branch or worktree the user named, or the open PR.
   Run `git worktree list`, `git status`, and
   `git log --oneline origin/main..HEAD` in it. Do not touch another
   agent's uncommitted changes unless the user hands them to you.
2. Read what was intended: the PR body, linked issue, commit messages, and
   any notes the user pointed at.
3. Compare done against intended. Name the first step that is not done.
4. Check that the done work still holds: rebase or merge state against
   `origin/main`, and the narrow tests for the changed crates. Do not redo
   work that passes.
5. Continue with the matching playbook from its first unfinished step.

**Reply:** where the prior work stopped, what you verified, what you did
next, and the outcome.
