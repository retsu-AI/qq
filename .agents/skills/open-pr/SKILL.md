---
name: open-pr
description: Commit, gate, push, and open a ready GitHub PR for finished work. Use when the user asks to open a PR, push, or ship a branch for review.
---

# Open a PR

Use only when the user asked for a PR or a push. The work is done and
reviewed with `review-diff`.

## Steps

1. **Branch.** Stay on the branch that holds the finished work. Check it
   with `git branch --show-current` and `git log --oneline origin/main..HEAD`:
   it must be your own branch, named `<type>/<eng-id>-<short>` (drop
   `<eng-id>-` when there is no Linear issue), and not `main`. If you
   committed on `main`, run `git switch -c <type>/<short>` to carry the
   commits to a new branch. If the branch is another agent's, stop and
   ask. If `git` reports no repository, you are in a QQ Auto task copy:
   use `task_commit`, `task_push`, and `task_pr` for steps 3 to 5
   instead.
2. **Commits.** Small, ordered commits, each one building and passing its
   tests. Titles are Conventional Commits, `type(scope): subject`, with a
   lowercase imperative subject and no trailing period. Use the crate or
   area as scope: `fix(core): keep the pending request on retry`. A body
   says why, not what the diff says. Check `git status` first and stage
   only your files; never stage `target/`, `.env`, or credentials.
3. **Gate.** Rebase onto the latest `origin/main`, then run
   `cargo xtask check` once on the committed tree. If
   `git config core.hooksPath` prints `.githooks`, the pre-push hook runs
   it for you in step 4. On failure, read `target/qq-check/last.json`,
   run its rerun command while you fix the cause, commit, and gate again.
4. **Push.** `git push -u origin HEAD`. Never `--force`. A rejected push
   means someone else pushed: fetch and rebase, or ask.
5. **Open it ready, not draft.** Write the body to a file and run:

   ```sh
   gh pr create --base main --title "<conventional title>" --body-file <file>
   ```

   The body follows `.github/pull_request_template.md`, in short plain
   sentences:

   ```markdown
   ## Change
   What changes for users and why. `Closes ENG-123` when there is an issue.

   ## Verification
   - `cargo nextest run -p qq-core -E 'test(/pending_request/)'`: failed before, passes after
   - `cargo xtask check`: passed on <short sha>
   - The surface you exercised and what you observed

   ## Known impact
   Regressions, compatibility changes, budget overruns, or unverified behavior. "None" is fine.
   ```

   `cargo xtask check --evidence` prints the gate result for the current
   commit if you want to paste it.
6. **Confirm.** `gh pr view --json url,isDraft,headRefOid` shows the URL,
   `isDraft: false`, and the head you pushed. Codex reviews new PRs on its
   own.

Opening a PR does not start babysitting. If the user also asked for green
CI, review fixes, or a merge, load `babysit-pr` next.

**Reply:** the PR URL and one line on what it changes.
