# Desktop production readiness ledger

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| Analysis | Source comparison and plan | Complete | feat/desktop-production-readiness; local only | Five clones refreshed, source inspected |
| DP1 | Bounded commands, durable submission | Implemented; live qualification partial | same | 5 Rust and 2 draft tests; offline live state |
| DP2 | Run controls and recovery | Implemented; live qualification partial | same | Steer/interrupt mapping; startup retry; visible errors |
| DP3 | Session organization | In progress | same | Local aliases/archive, search |
| DP4–DP8 | Pickers through release qualification | Planned | same | See owning plan |

## 2026-10-17 — Start and comparative analysis

- Base 8170d4f7 on feat/desktop-app; created feat/desktop-production-readiness.
- Protected original edits: AGENTS.md and apps/desktop/src-tauri/capabilities/default.json.
- Read workflow, architecture, protocol, ADR-0042/0062, multi-surface plan and desktop README.
- All five reference `git pull --ff-only` operations succeeded: already current.
  Read-only parallel source investigation; no copied code or reference builds.
- Baseline: desktop cargo test --locked 1 passed; cargo fmt --check,
  cargo clippy --locked --all-targets -- -D warnings, pnpm typecheck passed.
  Existing dependency dead-code warnings in qq-auth/qq-core; no desktop errors.
- DP1 findings: unbounded UiCommand channel; composer clears before server
  acknowledgement; event-before-receipt removes pending prompts; no offline guard.
- No engine edits, provider calls, credentials/token reads, pushes or PR changes.

## 2026-10-17 — DP1/DP2 implementation

- UI channel and pending receipts bounded at 64; saturation is explicit.
- Prompt IPC waits for the durable receipt, including create-then-submit;
  events cannot discard the waiter. New sessions focus after prompt acceptance.
- Draft regression coverage: rejection/retry, duplicate submission and late
  acknowledgement after editing/switching. Node's built-in runner adds no dependency.
- Explicit queue/steer/interrupt selector uses advertised native capabilities;
  finished-run steering rejects; cancellation failures are visible.
- Startup failures can retry. Landing errors and disconnected transcript state
  are visible with alert/status semantics; submission pauses while disconnected.
- Checks: Rust tests 5 passed, Clippy and formatting passed; typecheck passed;
  draft tests 2 passed. Engine build passed after closing its supervised instance.
- Live Windows UI Automation: throwaway qq-desktop-readiness-20261017 project;
  discovered HTTP 500 workspace bootstrap previously hidden by Connecting text.
  Error now visible. Connected/provider/control live qualification remains open.
- No model calls or engine source changes. Two protected edits unchanged.
