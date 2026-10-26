# Feature

You own the design. Build the smallest version that gives the user the
behavior they asked for.

1. Read how the affected subsystem works today. Start from the crate in
   the `AGENTS.md` repository map and the matching `docs/design/` page.
   Delegate the reading if it spans many files.
2. Name the data shape first: the types, states, and who owns them. Prefer
   an enum or state machine over flags, and a table over branching. Write
   the shape down in two or three lines before you write logic.
3. Delete before you add. Remove the dead code, one-caller wrappers, and
   stale paths the change makes obsolete.
4. Build it in small steps. Each step compiles and keeps the narrow tests
   green. Commit each step that stands on its own.
5. Test the behavior a user observes, against literal expected values.
   Then exercise it on the real surface with `verify-qq` or an existing
   `cargo xtask smoke` scenario.
6. If the change makes `docs/guide/` or `docs/design/` wrong, fix them in
   the same branch. Wire-type changes follow the protocol rules in
   `AGENTS.md`.
7. Load `review-diff` and fix what it finds.
8. If the user asked for a PR, load `open-pr`.

**Reply:** what users can now do, the shape you chose and the main
alternative you rejected, and how you verified it.
