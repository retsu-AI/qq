# F14 — exact Windows test coverage

| Slice | Goal | Status | Branch / PR | Inputs |
| --- | --- | --- | --- | --- |
| F14 / ENG-784 | Never silently qualify zero selected tests | In progress | `ci/eng-784-f14-exact-test-coverage` | main `d4fd971` |

## Acceptance and ownership

Own `.github/workflows/ci.yml`, `.github/scripts/test-exact.sh`, the Windows
CI runbook, and this ledger. Root authorizes the CI change (request recorded
in `progress/root.md`). No runtime, dependency, schema, or protocol changes;
no product performance gate is affected.

Keep nine named native Windows steps. Require successful Cargo exit, a named
passing case, and exactly one passing/non-ignored libtest result. Reject zero,
ignored, multiple, failed, malformed, and command-failure outcomes. Correct
the five stale delegation module paths. Run native workflow dispatch before
PR; local Linux checks do not qualify Windows. Full workspace gates before
push; independent source and receipt review.

## 2026-09-16

- Revalidated five stale selectors on current main; four tool paths remain
  correct. Independent read-only review confirmed module paths and cfgs.
- Created and read ENG-784 via CLI after the Linear integration rejected auth.
- Added the coverage assertion first; existing workflow paths are unchanged
  until the assertion demonstrates the original zero-test failure.
- Baseline Cargo test compilation is running in this isolated worktree.

- Baseline complete: stale unconfirmed-shell selector returns exit 0 with
  zero tests (603 filtered). The new assertion returns exit 1 for that exact
  selector. No production sources changed; fresh debug build at `d4fd971`.
