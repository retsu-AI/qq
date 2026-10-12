# Playbook: test audit

Goal: fewer, faster tests that catch more. Delete with evidence, never in
bulk.

1. **Pick one module** (for example `crates/qq-core/src/sessions/tests/compaction.rs`).
   Record its test count and time:
   `cargo nextest run -p <crate> -E 'test(/^<module path>::/)'`.
2. **Find flakes.** Run the module 20 times under load:
   `cargo nextest run -p <crate> -E '...' --stress-count 20` (or a shell loop
   while `cargo xtask check --full` runs elsewhere). Every failure is a bug in
   the test or the code; reproduce it like `playbooks/bug-fix.md` step 1.
3. **Remove fixed sleeps.** Each `tokio::time::sleep`/`thread::sleep` that
   `cargo xtask lint` counts is either waiting for something (replace with
   the signal, channel, or `Notify` it is waiting for) or advancing time
   (use `#[tokio::test(start_paused = true)]` and `tokio::time::advance`).
   Wall-clock upper bounds (`elapsed < 30ms`) move to a bench or a perf
   budget. Lower `SLEEP_CEILING` in `xtask/src/lint.rs` by the number you
   removed, in the same commit.
4. **Measure value with mutants.** `cargo mutants -p <crate> -f <file>`
   (install with `cargo install cargo-mutants` if missing). A test that
   kills no mutant another test does not also kill is a deletion candidate.
5. **Merge duplicates into tables.** Near-identical tests across files
   (for example the per-protocol provider tests) become one table-driven test
   next to `crates/qq-provider/tests/interface/canary.rs`.
6. **Delete trivia.** A test that would still pass if the function under test
   returned a default, or that asserts a constant, goes. Load `principles`,
   *Test behavior, not implementation*.
7. **Prove no loss.** The mutation catch count for the module did not drop;
   the module is faster; `cargo xtask check` passes.
8. Run `playbooks/opening-a-pr.md`: one module per PR, with before/after
   test count, time, mutants caught, and each deleted test named with its
   reason.
