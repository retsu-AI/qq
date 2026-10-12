# Playbook: bug fix

You own the fix. Every shipped line traces to runtime evidence. A change that
"might help" is a hypothesis; it does not ship. When evidence refutes a
hypothesis, revert what it motivated.

1. **Reproduce on the user's surface.** Pick the cheapest one that shows the
   bug: an existing smoke scenario (`cargo xtask smoke --only <name>`), a new
   smoke scenario in `xtask/src/smoke.rs`, a `verify-qq` drive, or a failing
   test at the narrowest public boundary. Record the failing output verbatim.
   If it will not reproduce, tighten conditions or add instrumentation until
   it does; do not fix a bug you have not seen.
2. **Binary-search the cause.** List candidate mechanisms; each pass, run
   the check that eliminates the most. Read the events
   (`qq run --format jsonl`), the provider requests
   (`target/qq-smoke/<scenario>/provider-requests.txt`), and the session store
   (`docs/runbooks/run-output-queries.md`). Confirm the surviving mechanism
   with evidence before writing the fix.
3. **Commit the failing test first** (`test(<scope>): reproduce <bug>`), so
   history shows red before green. Every fix has one. Use the cheapest test
   that fails for the right reason: prefer adding a case to an existing table
   or scenario over a new test function.
4. **Fix at the root.** The smallest change the evidence justifies. No
   defensive guards that silence the symptom, no retries that hide it, no
   sleeps.
5. **Verify on the same surface.** The step-1 repro now passes; paste
   failing-then-passing output. A unit test passing elsewhere is not this.
6. **Run `cargo xtask check`.** Fix everything it reports.
7. **Ask: could a rule have caught this?** If yes, run the correct playbook
   in a separate commit (`.qq/skills/qq-mode/playbooks/correct.md`).
8. Run `.qq/skills/qq-mode/playbooks/opening-a-pr.md`.

**Reply:** what was broken (user-visible), the root cause with
`file:line`, the fix, failing-then-passing output, and the evidence block.
