# Desktop production readiness ledger

| Slice | Goal | Status | Branch / PR | Notes |
| --- | --- | --- | --- | --- |
| Analysis | Source comparison and plan | Complete | feat/desktop-production-readiness; local only | Five clones refreshed, source inspected |
| DP1 | Bounded commands, durable submission | Implemented; live qualification partial | same | 5 Rust and 2 draft tests; offline live state |
| DP2 | Run controls and recovery | Implemented; live qualification partial | same | Steer/interrupt mapping; startup retry; visible errors |
| DP3 | Session organization | Implemented; fixture verified | same | Search, reversible local aliases/archive |
| DP4 | Pickers and workspace files | Implemented; native live qualification blocked | same | Shared mention resolver; typed files and session options |
| DP5 | Settings, diagnostics and palette | Implemented; fixture verified | same | Modal focus isolation, preferences and shortcut guards |
| DP6 | Single instance and notifications | Implemented; notification qualification pending | same | Native second-process forwarding verified; installed toast check outstanding |
| DP7 | Virtualization and generated types | Implemented; fixture verified | same | 2048 retained messages, 11 mounted rows; contract drift test |
| DP8 | CI, packaging and docs | Final checks in progress | same | Windows CI, metadata guard, release/signing guide |
| DP9 | Additional polish | Out of scope | same | P2 tray/key remapping/richer diff navigation recorded in decisions |

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

## 2026-10-17 — DP3 session organization

- Local aliases and reversible archive are explicitly labelled desktop-local;
  histories remain canonical. Active/needs-you sessions remain visible.
- Case-insensitive multi-term search includes aliases and recent output;
  search exposes matching child sessions. Metadata bounded at 2048 records.
- Tests: persistence/reload/restore, malformed metadata, long names and search.
- Live isolated 512-session fixture: searched Task 511, renamed it Release
  smoke fixture, archived it, opened Archived locally, restored it.
- Desktop fmt, Clippy, 8 Rust tests, typecheck, 8 JS tests and production UI
  build passed in the integrated checkout. No engine or protocol changes.

## 2026-10-17 — DP4–DP7 integration and independent review

- Added existing-session profile, advertised effort and approval-delegate controls,
  and child/compact/rollback/prune commands over existing protocol commands.
- Workspace-file inputs reuse qq-core's bounded contained mention resolver off
  Tokio workers, with bounded concurrent preparation. Invalid/unresolved inputs
  preserve drafts. Binary/image uploads remain outside the current protocol.
- Settings expose theme, 12–22 px text, shortcuts, opt-in unfocused notifications,
  connection/engine state and reported MCP hosts. Configuration mutation remains
  through QQ config/trust; the UI does not claim to manage server configuration.
- Single-instance second process exited successfully and forwarded a second
  throwaway folder into the existing QQ window. No duplicate application window.
- Generated view DTO contract uses ts-rs with a checked-in drift test. No root
  Cargo manifest/lock, engine crate source or protocol version change.
- Independent read-only native/UI reviews found and resolved receipt-before-SSE
  cold-history adoption, actor shutdown during connect, startup identity reset,
  modal approval key leakage and remounted composer acknowledgement handling.
  Both final reviews reported no remaining actionable findings.
- Native adoption now requests an authoritative focused snapshot; it never marks
  missing history warm. New regression cases cover both prompted and empty child
  creation. Dropping the final project handle cancels connection/bootstrap work.
- Isolated fixture checks: profile reviewer and effort high emit typed session
  options; steering emits submit_prompt mode steer and clears after mock receipt.
  Settings theme/font applies; 22 px reached rendered assistant text. Pressing Y
  in Settings emitted no approval; after closing/focusing history, Y emitted one.
- Dynamic-height virtualization retains focused rows, bounds tool groups at 32,
  and reaches both ends with Ctrl Home/End. Fixed the initial null scroll-element
  mount race found by the live fixture. Shared draft subscriptions handle remounts.
- Diagnostic dev-build pair with 512 sessions / 2048 messages: baseline mounted
  2048 rows, first render 3463.8 ms; candidate mounted 11, first render 737.9 ms.
  Raw ignored evidence: apps/desktop/src-tauri/target/qq-perf/render-diagnostic.json.
  This single loaded-host pair is not a qualified tail-latency benchmark.
- Final frontend typecheck and all 10 Node tests passed. Production Vite build
  passed (486.47 kB JS before gzip). Final native and installer gates follow.
- Final native gates passed: fmt, Clippy with -D warnings, all 13 Rust tests
  and debug build. Existing path-dependency Windows dead-code warnings remain.
- Native integration committed as DP4. Settings/composer integration and
  modal approval guards are the next independently reviewable commit.

## 2026-10-17 — Qualification blockers and safety boundaries

- Native live launch in a throwaway project reproduces HTTP 500 workspace
  bootstrap. A read-only SQLite quick_check of the existing user sessions.sqlite3
  reports invalid pages, duplicate page references and out-of-order rowids.
  No repair, replacement, reindex, history inspection or credential/token read.
- Retry visibly transitions through Starting and reports bootstrap failure again;
  settings diagnostics remain usable. Connected native runs, real approval flows,
  provider calls and installed OS notifications cannot be qualified against this
  store. They remain explicit release gates after human-directed recovery.
- First simultaneous workspace-test/release attempt exhausted this 16 GB host:
  rustc/LLVM out-of-memory and compiler process failure. Retried packaging with
  CARGO_BUILD_JOBS=1; engine release build then succeeded in 13m04s. This is a
  verification-environment failure, not evidence of a test assertion failure.
- Signing certificate/authorized timestamp service and disposable install/upgrade
  qualification are human-supplied release requirements. Automatic updates,
  cross-client rename/archive and MCP mutation remain explicitly deferred.
