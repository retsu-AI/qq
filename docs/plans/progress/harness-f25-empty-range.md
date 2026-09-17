# F25 — empty ranged attachments

| Slice | Goal | Status | Branch / PR | Inputs |
| --- | --- | --- | --- | --- |
| ENG-783 / F25 | Reject nonexistent attachment lines without a worker panic | In progress | `fix/eng-783-empty-attachment-range` | main `83446d0` |

Own `qq-core/src/input.rs`, focused session tests, attachment documentation,
and this ledger. Public seam: SessionRuntime commands/events and requests to
the external provider. Empty whole files remain valid; ranges require an
existing starting line, and ends clip to EOF. Cover empty files, LF/CRLF,
EOF clipping, and extreme ranges in debug/release. Independent review and
workspace gates precede PR. No new allocation, I/O, dependency, or timing gate.

## 2026-09-17

- Revalidated ENG-783 and current main after cleanup; old notes remain stashed.
- Added the public-session regression before implementation. It checks typed
  failure, absence of provider work, and subsequent session usability.
- Baseline test pending; no implementation edits yet.

- Red baseline on `83446d0`: one test ran and failed. Empty line 1 triggered
  subtraction overflow on the blocking worker and surfaced `Server` instead
  of `InvalidCommand`. Runtime shutdown completed. Log `/tmp/qq-f25-red.log`.

- Red commit `e45c4d8`; changed only the range-start comparison in production.
  Both public-session tests pass in debug, including eleven range fixtures.
  Empty whole files remain valid; EOF clipping and line bytes are unchanged.
- Release-mode verification is running; independent final review requested.
  Full workspace gates, final receipt, push and PR are still pending.

#### F25 verification receipt — 2026-09-17

- Red commit: `e45c4d8`; two public-session regression tests added.
- Debug and release `cargo test -p qq-core attachment_range -- --nocapture`:
  2 passed each (release adds `--release`); eleven edge-case fixtures covered.
- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`: passed.
- `cargo build --workspace`: passed.
- `cargo test --workspace --quiet`: 1,490 passed, 0 failed, 5 ignored.
- Test environment: `NO_COLOR` unset, `TERM=xterm-256color`; dev/test/release
  debug symbols disabled through Cargo profile environment overrides.
- Independent source/spec review: approved, conditional on these now-passed gates.
- No named performance gate or boundary/schema change; one predicate correction,
  no added allocation, I/O, dependency, or loop. No measured speedup claimed.
- Evidence: `/tmp/qq-f25-{red,release,clippy,build,workspace}.log`; PR pending.
