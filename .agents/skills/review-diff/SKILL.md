---
name: review-diff
description: Review a branch diff for bugs, scope creep, weak tests, and rule violations before it ships. Use before opening a PR, before merging, or when asked to review changes.
---

# Review a diff

Find what would make a careful reviewer block the change. Report; do not
rewrite the diff while you review.

1. **Scope.** `git diff origin/main...HEAD --stat`, then the full diff.
   Write one sentence for what the change is meant to do, taken from the
   request, the commits, or the PR body.
2. **Fresh eyes.** For a diff over about 100 lines, or anything touching
   concurrency, persistence, approval, sandboxing, or the wire protocol,
   spawn one read-only sub-agent, on the `strong` role when the roster
   has one. Brief it with the intent sentence, the base and head commits,
   the list below, and this answer shape: findings as `file:line`, the
   problem, and the evidence. It cannot load skills, so put the list in
   the brief. Review the diff yourself while it runs.
3. **Check each item.**
   - Does it do what was asked, and nothing else? Flag unrelated edits,
     reverted lines, and leftover debug output.
   - Is each new or changed test a behavior test? It must fail if the
     change were reverted, assert literal expected values, and use no
     fixed sleeps or wall-clock bounds. Flag any weakened or deleted
     assertion.
   - Does a bug fix have a test that failed before it?
   - `AGENTS.md` code rules: no `mod.rs`, no `unsafe`, bounded channels,
     no lock across `.await`, no blocking I/O on Tokio workers, session
     events persisted before published, `thiserror` errors with sources,
     exhaustive `match` with no silent `_`.
   - Could it be smaller? Flag new abstractions with one caller, options
     no one sets, and compatibility paths with no user.
   - Wire types changed: `PROTOCOL_VERSION` bumped and
     `crates/qq-protocol/tests/fixtures/` regenerated.
   - Docs: does the change make `docs/guide/` or `docs/design/` wrong?
   - Secrets, `target/`, or local config committed.
4. **Verify findings.** Open every cited `file:line`, yours and the
   sub-agent's. Drop any finding you cannot point at in the code.

**Reply:** findings in two groups, **Fix** (correctness, security, a rule
broken, a missing test) and **Consider** (smaller, cleaner, clearer), each
with `file:line` and one line of reason. Say "no findings" when there are
none. Then fix the **Fix** group if the user asked for the change.
