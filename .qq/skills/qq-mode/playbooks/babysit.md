# Playbook: babysit

Drive an open PR to merge-ready without stopping at the first red check.

1. Read the PR, its linked issue, every review thread, and the latest CI run
   (job summary carries the `cargo xtask check` evidence; failed runs upload
   `qq-check-<run>` with `last.json`, JUnit, and smoke evidence).
2. **Triage each finding** as fix, dismiss (with a concrete reason in the
   thread), or ask. Bots file real bugs and non-issues; judge each on its
   merits.
3. **CI failures:** download the artifact, read `last.json`, reproduce
   locally with the rerun command it gives. A test that fails in CI but not
   locally is a flake: reproduce under load (`playbooks/test-audit.md`
   step 2) and fix it; do not re-run until green and move on.
4. **Conflicts:** rebase onto `origin/main`, rerun `cargo xtask check`.
5. Push fixes as new commits; reply to each thread with the commit.
6. Stop when every check is green, every thread is resolved or answered,
   and the PR is in the merge queue or waiting on a human approval only.
