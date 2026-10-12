# Playbook: feature

1. **Read before you design.** The issue, `docs/design/architecture.md` for
   any boundary you cross, and the design doc for the subsystem. Find the
   existing type or table the feature extends; most features here are a new
   variant, field, or row, not a new module.
2. **Name the data shape and its compatibility cost** in two or three
   sentences before writing code: the new or changed types, where they are
   persisted, and whether a wire type in `qq-protocol` changes. A wire change
   bumps `PROTOCOL_VERSION` and regenerates fixtures
   (`QQ_UPDATE_FIXTURES=1 cargo nextest run -p qq-protocol`); a store change
   is a schema migration. Either one needs the plan to name it; stop and ask
   if it does not.
3. **Write the acceptance as tests first.** One test per user-observable
   behavior and one per failure mode, at the public boundary
   (`SessionRuntime`, the CLI, the server API, the provider trait). If users
   reach the feature through `qq`, add a smoke scenario or extend one.
   Commit them failing.
4. **Implement the smallest version that passes.** No speculative options,
   no generic extension points, no new crate (`AGENTS.md` § Engineering
   Priorities).
5. **Prove it on the real surface.** `cargo xtask smoke`, and the matching
   `verify-qq` feature file for anything smoke does not cover. Update that
   feature file if the way to reach or drive the feature changed.
6. **Measure if it touches a hot path** (startup, first token, streaming,
   persistence, rendering): `playbooks/perf.md` steps 1 and 5.
7. **Document as built.** Amend `docs/design/` and `docs/guide/` in the same
   PR; `src/docs_truth.rs` tests fail when the guide misses a CLI flag.
8. **Run `cargo xtask check`.**
9. Run `playbooks/opening-a-pr.md`.

**Reply:** what a user can now do and how, the data shape and compatibility
consequence, evidence block, and smoke or `verify-qq` proof.
