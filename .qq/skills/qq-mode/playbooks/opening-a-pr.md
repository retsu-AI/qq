# Playbook: opening a PR

Every other playbook ends here.

1. **Clean history.** Small commits in Conventional Commits form
   (`type(scope): imperative summary`), the failing test before the fix,
   no fixups. Never commit `target/`, credentials, or generated evidence.
2. **Gate.** `cargo xtask check` passes on the final head. For user-visible
   changes, `cargo xtask smoke` passes too.
3. **Evidence block.** `cargo xtask check --evidence` prints Markdown for
   the PR body. Paste it; do not retype results. It refuses a report for a
   different commit or tree state, so commit first, then check, then paste.
4. **Branch name** `<type>/<eng-id>-<short-kebab>` (`<type>/<short-kebab>`
   without an issue); push with
   `git push -u origin HEAD` (the pre-push hook reruns `cargo xtask check`
   when enabled; see `docs/runbooks/testing.md`).
5. **PR body** (`docs/plans/templates/slice.md` for plan slices), in this
   order: user-visible behavior; design decisions and alternatives
   rejected; how verified (evidence block, smoke scenarios, failing-then-
   passing output for fixes); performance and compatibility impact
   (protocol version, store schema, config); follow-ups.
6. **Title** = the main commit subject. Add `!` and an impact paragraph for
   breaking changes.
7. **Link the Linear issue** (`ENG-<n>` in the title or body) and move it to
   In Review.
8. CI on the PR runs the affected gate; the merge queue runs the full gate.
   Add the `ci:full` label to run Windows and wasm on the PR itself when the
   change touches process teardown, paths, or `qq-client` transport.
