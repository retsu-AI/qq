# Preparation cancellation takeover (PR #293)

| Slice | Goal | Status | Branch/PR | Notes |
| --- | --- | --- | --- | --- |
| preparation | Cooperatively stop later preparation phases without releasing ownership early | Blocked (line-pinned legacy sleep baseline) | fix/qq-preparation-deadline-v0.1.6 / #293 | Implementation ready for independent review; no push |

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


### Takeover verification

- Red: `cargo nextest run -p qq-core -E 'test(preparation_cooperates_with_deadline_and_explicit_cancel)'` failed with `loader must receive cooperative cancellation`, left `0`, right `1`. Committed as `e889bd41` (additive signal API plus repro, before core signalling).
- Green: 28 focused deadline, routing, and production cancellation tests passed. Cooperative initial/reload deadline/cancel cases use entered/drained signals and paused deadline advancement. Legacy loader test observes cancellation but retains its session reservation and execution permit until manual release; no provider requests occur.
- Production keyring cancels during selected credential read: later provider/tool/plan phases are suppressed, typed `PreparationCancelled` returned, and a fresh uncancelled lookup reports `Compiled`, proving no cancelled generation was cached. Pre-cancelled load rejects before workspace blocking work.
- Full affected package test selection: 1948 passed, 13 skipped. Affected-package Clippy (`--all-targets -- -D warnings`) passed. `cargo xtask smoke`: 6 passed, 0 failed (rerun after routing test addition).
- `cargo xtask check --base origin/main` fails lint because legacy sleep entries pin exact source line numbers. Seven unchanged sleep sites moved in the two owned Rust files; baseline requires seven line-number updates in `xtask/src/legacy-sleeps.txt`, outside assigned ownership. No sleeps were added. Report: `target/qq-check/last.json`. Parent must authorize/update that baseline before the complete gate can pass; no gate was weakened.
- Scope remains six owned files. No protocol/schema/dependency changes; no syscall interruption, early settlement, or cancellation of cache single-flight waits promised. Cancellation racing a fully completed generation can leave that complete generation cached; incomplete/cancellation-observed compiles return before cache publication.
