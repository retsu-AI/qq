# Playbook: babysit

Drive an open PR to merge-ready without stopping at the first red check.

1. Read the PR, its linked issue, every review thread, and the latest CI run
   (job summary carries the `cargo xtask check` evidence; failed runs upload
   `qq-check-<run>` with `last.json`, JUnit, and smoke evidence).
2. **Triage each finding** as fix, dismiss (with a concrete reason in the
   thread), or ask. Bots file real bugs and non-issues; judge each on its
   merits.
3. **CI failures:** download the artifact
   (`gh run download <run-id> -n qq-check-<run-id> -D target/ci-<run-id>`),
   read `jq '.gates[] | select(.passed|not)' target/ci-<run-id>/qq-check/last.json`,
   and reproduce locally with the rerun command it gives. A test that fails in CI but not
   locally is a flake: reproduce under load (`.qq/skills/qq-mode/playbooks/test-audit.md`
   step 2) and fix it; do not re-run until green and move on.
4. **Conflicts:** rebase onto `origin/main`, rerun `cargo xtask check`.
5. Push fixes as new commits; reply to each thread with the commit.
6. Stop when CI is green and review threads are answered. Then tell the
   user "mergeable" in one line. A bot's written "no major issues" counts as
   approval; don't wait for emoji reactions.
