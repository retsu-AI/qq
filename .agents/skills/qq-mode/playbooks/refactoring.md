# Playbook: refactoring

Behavior must not change. The proof is that nothing observable moved.

1. **Baseline.** On the unmodified branch run `cargo xtask check --crate <pkg>`
   and `cargo xtask smoke`; keep the "Tests" column of
   `cargo xtask check --evidence`. For a hot path, record the
   `.qq/skills/qq-mode/playbooks/perf.md` baseline too.
2. **Subtract first.** Delete dead code, one-caller wrappers, and duplicate
   tests the refactor makes redundant before moving anything.
3. **Move in verifiable steps.** Each commit compiles and passes
   `cargo xtask check`. Migrate every caller and delete the old API in the
   same change; no compatibility shims.
4. **Do not add tests to a pure refactor** unless the baseline showed a gap
   the refactor would otherwise hide. Do not edit assertions; if a test must
   change, the refactor changed behavior: stop and reassess.
5. **Prove sameness.** Same test count passing (minus deleted duplicates,
   listed), smoke unchanged, protocol and headless fixtures unchanged
   (`git diff --stat crates/qq-protocol/tests/fixtures` is empty).
6. Run `.qq/skills/qq-mode/playbooks/opening-a-pr.md`.

**Reply:** the structural change and why it is simpler for the next
maintainer, line delta, what was deleted, and the before/after counts.
