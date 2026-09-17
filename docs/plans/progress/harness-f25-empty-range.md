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
