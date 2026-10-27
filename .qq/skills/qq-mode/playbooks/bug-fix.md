# Bug fix

You own the root cause. Every shipped line traces to something you
observed.

1. Reproduce it on the surface the user hit: the CLI, the TUI, the server,
   or the library API. Use `verify-qq` for binary behavior. If it will
   not reproduce, tighten the conditions or add logging until it does.
   Do not fix what you cannot see.
2. Find the mechanism. List the candidate causes, then rule them out with
   evidence: logs, a debugger, a narrower test, `git log -S` or
   `git bisect` for regressions. Stop when one cause survives and you can
   point at it with `file:line`.
3. Write the narrowest behavioral test that fails because of that cause.
   Run it and keep the failing output.
4. Fix the cause where it starts. A guard that hides the symptom (a
   default over an unexpected `None`, a retry around a race, a sleep) is a
   second bug. If evidence refutes a change you made, revert it.
5. Run the test again and keep the passing output. Repeat step 1's
   reproduction on the same surface. A passing unit test does not prove
   the user-facing bug is gone.
6. Commit the test and the fix. If the user asked for a PR, load
   `open-pr`.

**Reply:** what was broken for the user, the root cause at `file:line`,
the fix, and the failing-then-passing test output.
