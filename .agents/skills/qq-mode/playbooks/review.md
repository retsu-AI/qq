# Playbook: review

Review someone else's PR. The process contract is `docs/plans/workflow.md`
§4 and §8. The checklist is `docs/plans/templates/review-checklist.md`.

1. Read the PR, its linked issue, and the plan or slice section it
   implements.
2. Check out the head in your own worktree:
   `git worktree add .worktrees/review-<n> <pr-head-sha>`.
3. Run the gates yourself: `cargo xtask check --base origin/main` and, for
   user-visible changes, `cargo xtask smoke`. Do not trust the PR's
   evidence block without reproducing it.
4. **Spec conformance.** Does the PR do what the slice says, no more, no
   less? Missing acceptance, unmeasured named gates, and edits outside the
   owned paths are blocking.
5. **Standards conformance.** Check `AGENTS.md` § Rust style, bounded
   resources, no lock across `.await`, typed errors, persist-before-publish,
   and tests that assert behavior (`principles` skill, *Test behavior, not
   implementation*).
6. **Try to break it.** Run the changed path with inputs the tests do not
   cover. Use the `verify-qq` skill for user-visible behavior.
7. Report the verdict (`Approve`, `Request changes`, or `Escalate`), then
   findings ordered blocking, should-fix, nits. Give each finding a
   `file:line` and a concrete fix. If the same finding recurs across PRs,
   propose a rule (`.qq/skills/qq-mode/playbooks/correct.md`).
8. Remove the worktree: `git worktree remove .worktrees/review-<n>`.
