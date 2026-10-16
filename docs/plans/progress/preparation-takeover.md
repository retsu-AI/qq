# Preparation cancellation takeover (PR #293)

| Slice | Goal | Status | Branch/PR | Notes |
| --- | --- | --- | --- | --- |
| preparation | Cooperatively stop later preparation phases without releasing ownership early | In progress | fix/qq-preparation-deadline-v0.1.6 / #293 | Existing worktree takeover authorized by parent; no new PR or push |

## Procedure

1. **Reproduce on the user's surface.** Narrow SessionRuntime deadline regression.
2. **Binary-search the cause.** Initial and routing loaders are drained but receive no cancellation signal.
3. **Commit the failing test first** (`test(<scope>): reproduce <bug>`).
4. **Fix at the root.** Add cancellation to existing per-load progress; production phase checks.
5. **Verify on the same surface.** Focused deadline and production tests.
6. **Run `cargo xtask check`.** Use `--base origin/main`, then smoke.
7. **Ask: could a rule have caught this?** Behavioral regression tests enforce the contract.
8. Run `.qq/skills/qq-mode/playbooks/opening-a-pr.md`: skip new PR/push; parent owns existing PR updates and independent review.

## Evidence

- Read current cache: compilation errors return before any generation publication; check cancellation before returning the complete generation. Cache hits and single-flight waits still drain normally.
- No syscall interruption or early settlement is promised. Existing custom loaders can ignore the additive signal.
