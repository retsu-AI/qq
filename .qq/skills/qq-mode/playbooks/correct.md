# Playbook: correct (encode a recurring mistake)

Run when the same correction happens twice: from a reviewer, the user, CI,
or your own rework.

1. **Name the mistake class** in one line ("tests wait with fixed sleeps",
   "new `mod.rs` files", "PR claims tests pass without evidence").
2. **Find the highest enforcer that works**, in this order:
   1. Make it impossible: a type, a private constructor, a module boundary.
   2. Make it fail fast: a Clippy lint in `[workspace.lints]`, a rule in
      `xtask/src/lint.rs` (`Rule` enum, `evaluate`, a test in its `tests`
      module), or a CI step.
   3. Make it fail in tests: an assertion at the boundary that owns it.
   4. Only then, prose: one line in the right skill or `AGENTS.md`.
3. **Existing violations:** if fixing them all is out of scope, add a ceiling
   (like `SLEEP_CEILING`) set to today's count. New violations fail; the
   ceiling only moves down.
4. **Add a row** to the `AGENTS.md` § Rules → enforcers table: the rule, its enforcer.
   Delete any prose the enforcer now makes redundant.
5. Test the enforcer: it fails on a known bad example and passes on the
   tree.
6. Run `playbooks/opening-a-pr.md` as its own PR (`build(lint): ...`).
