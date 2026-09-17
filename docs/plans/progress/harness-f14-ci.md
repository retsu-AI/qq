# F14 — exact Windows test coverage

| Slice | Goal | Status | Branch / PR | Inputs |
| --- | --- | --- | --- | --- |
| F14 / ENG-784 | Never silently qualify zero selected tests | In review | [PR #63](https://github.com/retsu-AI/qq/pull/63), `ci/eng-784-f14-exact-test-coverage` | main `d4fd971` |

## Acceptance and ownership

Own `.github/workflows/ci.yml`, `.github/scripts/test-exact{,-test}.sh`, the Windows
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

- Wired the guard into all nine steps and corrected five delegation paths.
  Corrected unconfirmed-shell selector executes one passing case on Linux.
  Ten guard fixture cases pass, including CRLF and command-failure propagation.
- Independent review approved source/portability and independently reran the
  fixture checks. Workspace gates and native Windows dispatch remain pending.
  Local workspace build disables debug symbols to limit disk consumption.

#### F14 local receipt — 2026-09-16

Base `d4fd971`; red guard commit `4ddb791`.
Guard: 10 fixture cases passed; stale selector rejected; 7 Linux-eligible exact cases passed.
Workspace: 1,466 passed / 5 ignored; fmt, all-target/all-feature Clippy `-D warnings`, build passed.
Commands: `cargo test --workspace --quiet`, `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo build --workspace`.
Environment: `NO_COLOR` unset, `TERM=xterm-256color`; dev/test debug symbols disabled for disk bounds.
Independent review approved; syntax and diff checks passed. Runtime code unchanged.
Docs: Windows CI runbook and root CI authorization. No product performance gate affected.
Logs: `/tmp/qq-f14-{workspace,clippy,build}.log`.
Open: native Windows dispatch before PR; no Windows or full audit completion claim yet.

#### F14 native receipt and review handoff — 2026-09-16

Implementation `9ba8a23`; workflow dispatch [35171415737](https://github.com/retsu-AI/qq/actions/runs/35171415737) succeeded before PR.
Native Windows job `105043698138`: all nine named cases passed under Rust 1.97.1.
Logs verified: nine separate `1 passed; 0 failed; 0 ignored` summaries and named test passes.
Hosted Linux formatting/guard fixtures/Clippy/tests and WASM checks also passed.
Published PR #63; not merged. Only nine targeted native cases are qualified, not the Windows workspace.
This final documentation update leaves implementation and workflow sources unchanged.
