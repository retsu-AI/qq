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
| DP8 | CI, packaging and docs | Implemented; NSIS built; install/live qualification pending | same | Windows CI, metadata guard, release/signing guide |
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
- Settings/composer and modal safeguards committed as DP5. The rendering
  fixture and virtualized history follow separately; package metadata is checked
  before staging the engine sidecar, with version parser regression coverage.

## 2026-10-17 — Qualification blockers and safety boundaries

- A corrupted local test store blocked initial workspace bootstrap. Store handling
  and diagnostics stayed outside git; retry visibly returned an actionable error.
- Connected runs, approvals and installed notifications remained qualification
  gates at this stage. Settings diagnostics stayed usable during the failure.
- Concurrent workspace checks and release compilation exhausted available memory.
  Retrying packaging with CARGO_BUILD_JOBS=1 succeeded. This was an environment
  failure, not a failed assertion.
- Signing requires a release identity and timestamp service. Automatic updates,
  cross-client organization and MCP mutation remain deferred.

## 2026-10-17 — Build checks and documentation

- DP7 commits virtualization and the development-only fixture. Added a
  Windows CI workflow with locked installs, frontend/native gates, generated
  contract validation and manual NSIS artifact retention; it never publishes.
- README and new guide cover controls, local metadata, input limits, diagnostics,
  retained-history behavior, manual upgrades and signing. ADR-0062 evidence is
  appended without changing its Decision. Architecture now describes the desktop.
- Root formatting passed. Root binary tests initially showed 15 failures because
  cmd could not find cp/rm. Retried with Git usr/bin on this process's PATH and
  --test-threads=1: 253 passed, 11 failed, 2 ignored, matching the supplied Windows
  baseline count. All four headless failures disappeared. Final failures:
  - docs_truth: every_compatibility_number_in_the_guide_is_this_builds;
    glossary_defines_each_term_with_the_builds_names_and_bounds;
    output_samples_in_the_guide_match_the_code;
    policy_table_marks_exactly_the_managed_only_keys;
    precedence_table_names_every_override_and_mdm_source;
    troubleshooting_headings_quote_real_messages; tui_command_table_is_the_registry.
  - runtime: pack_profiles_compile_persona_skills_tool_policy_and_mcp_subsets_into_plans;
    passive_advisory_does_not_gate_next_run_and_cancellation_retains_dispatch;
    plan_descriptor_covers_provider_model_workspace_and_tools_without_secrets;
    profiles_select_defaults_key_the_plan_cache_and_reject_unknown_names.
- These report CRLF-sensitive documentation comparisons, Windows canonical path
  expectations and a file-lock error. No engine source/root Cargo diff against
  8170d4f7. Raw logs: target/qq-perf/desktop-bin-tests{,-serial}.log (ignored).
  The supplied pre-change count is not an independently captured baseline run.
- Final pnpm check:version and production build passed. Desktop fmt/Clippy,
  13 Rust tests, 10 Node tests and typecheck passed before these local commits.
- Closed native test windows and supervised engine; stopped the fixture dev server.
  Closed the active fixture browser tab. A prior crashed temporary browser tab's
  close action was policy-blocked; it is left to normal temporary-tab cleanup.
- Additional shared reducer qualification: 24 of 25 state tests passed, including
  both notification ordering/deduplication cases. The sole failure,
  replaying_the_wire_fixtures_matches_the_golden_projection, compares LF output
  against a CRLF checkout fixture. Parsed both assertion strings from the raw log:
  equality after CRLF-to-LF normalization is true. No fixture/source rewrite.
  Raw ignored evidence: target/qq-perf/desktop-reducer-golden.log.

## 2026-10-17 — Final local implementation receipt

Result: the comparative analysis, desktop implementation, local checks and unsigned
release build are complete. **Production sign-off remains blocked** by the existing
a corrupted local test store and the connected/installed qualification gates below.
The branch is feat/desktop-production-readiness, based on 8170d4f7. Nothing was
pushed, published, merged, rebased or written to a PR.

### Shipped on the local branch

| Commit | Result |
| --- | --- |
| Analysis | Five-reference source comparison, prioritized slices and decisions |
| DP1 | Bounded submission admission and durable acknowledgement/draft handling |
| DP3 | Search and reversible, explicitly local session organization |
| DP4 | Native options/actions/files, actor recovery/lifetime, single instance, notifications and generated view contracts |
| DP5 | Settings, command palette, pickers, remount-safe drafts and modal approval safeguards |
| DP7 | Measured transcript virtualization, isolated fixture and release version guard |
| DP8 | Windows CI, guide/README, architecture and appended ADR-0062 evidence |

No engine/protocol change needs a separate upstream PR. Root src/, crates/,
Cargo.toml and Cargo.lock match the base. The native desktop remains its own
workspace and uses the shared runtime, transport and reducer. No reference code
was copied. All five consulted source revisions are recorded in the plan.

### Verification and package

- Desktop formatting, Clippy with -D warnings, 13 Rust tests, 10 Node tests,
  TypeScript typecheck, version guard, debug build and production UI build passed.
- pnpm bundle with CARGO_BUILD_JOBS=1 succeeded: engine release 13m04s, desktop
  release 12m09s, then NSIS. Artifact:
  apps/desktop/src-tauri/target/release/bundle/nsis/QQ_0.1.6_x64-setup.exe,
  16,761,583 bytes (15.99 MiB).
- NSIS packaging includes the staged qq.exe sidecar and consistent release
  metadata. The artifact is unsigned; signed distribution remains a release gate.
- Root formatting passed; binary tests with required cp/rm and serial execution:
  253 passed / 11 failed / 2 ignored, as detailed above. Shared reducer tests:
  24 passed / one proven CRLF-only golden failure. The attempted full-workspace
  run failed during compilation under memory pressure; it is not a passing gate.
- Native live: visible bootstrap failure/retry, diagnostics and single-instance
  forwarding verified in throwaway folders. Browser fixture: session organization,
  picker/steering IPC, settings, approval modal isolation and bounded history
  navigation verified. Mocked IPC is not evidence of connected engine execution.
- Performance evidence is the diagnostic pair above, not a quiet-host tail result.
- Native app, supervised engine, compiler/cargo and dev-server processes have
  exited; no listener remains on 1420. Generated build evidence is ignored.
  Only the two protected original edits remain after this receipt is committed.

### Deferred choices and remaining release gates

The plan records alternatives, reasons and reversal paths for local aliases/archive,
configured defaults on new sessions, existing-session pickers, contained text/files,
read-only MCP diagnostics, a typed desktop palette, generated DTO boundaries and
native dialog isolation. Binary/image upload, canonical cross-client organization,
MCP mutation and automatic updates require separately designed engine/release
support. P2 tray, arbitrary key remapping and richer diff navigation are out of scope.

1. Resolve the isolated qualification environment, then verify connected
   create/submit, file resolution, steering/interruption/cancel, approvals/questions
   and reconnect/replay against a throwaway project with a NoAuth loopback provider.
2. On a disposable Windows account/VM qualify fresh installation, upgrade and
   installed unfocused notifications. Hosted CI has been authored but not executed
   on GitHub because publication was prohibited.
3. Repeat large-history/streaming performance on a quiet host before assigning a
   latency budget. The current diagnostic establishes bounded mounting only.
4. For signed distribution supply an authorized signing identity, timestamp service
   and release host, then follow the README procedure and verify both executables
   and the installer. No signing identity or signed update feed is present.

These gates are not silently waived and this receipt does not certify the app as
fully production ready. All unaffected implementation work is committed for review.

## 2026-10-17 — Resumed connected qualification

- Live qualification ran against a throwaway project and a NoAuth loopback provider;
  a corrupted local test store was handled outside git.
- Native workspace bootstrap and provider SSE completed. A disposable @file marker
  reached the fixture; invalid mentions retained the draft. Held approval and
  structured question responses continued once. Settings blocked approval keys
  while the modal was open.
- A process-only QQ_CONFIG_CONTENT override isolated the provider fixture. No paid
  provider or stored credentials were needed.

### Native qualification and acknowledgement-focus regression

- Native UI/server/loopback checks passed: valid @file, invalid mention preserving
  the draft, held shell approval, structured question answer, profile/effort/
  delegate options, queued follow-up, boundary steering and interrupt-and-steer.
  Provider logs prove one queue/steer continuation, high effort on the next run,
  and cancellation of the interrupted HTTP stream with earlier text preserved.
- Found a real keyboard defect: disabled={pending} blurs the textarea during a
  receipt, so immediate follow-up typing and Escape no longer reach the composer.
  Replaced it with readOnly={pending} plus aria-busy; admission/duplicate guards
  remain. New eight-frame browser regression fixture reported FAIL before the
  change, PASS after. Native Escape cancelled a stream without refocusing after
  the fix, and normal typing continued into the next request.
- All desktop gates passed again: fmt, Clippy -D warnings, 13 Rust tests,
  typecheck, 10 Node tests and production frontend build. Release rebuilt with
  pnpm tauri build --config src-tauri/tauri.bundle.conf.json, using the unchanged
  hash-verified engine sidecar; optimized native build took 5m43s and NSIS succeeded.
- Forced exit of only the verified desktop-owned engine: UI showed Offline /
  catching up, a new owned engine started, and the client returned Connected.
  Reset returns to session selection; selecting the persisted session reloaded
  history. Full integrity_check stayed ok and foreign_key_check empty afterward.
- Empty child creation/adoption and a child model request passed. Local simulated
  401 rejection appeared in the transcript without credentials. Compaction's first
  fixture request was rejected by an overly strict QA tag guard; after supporting
  the real compaction instruction/required sections, compaction committed once,
  rollback returned the QA compaction count to zero, and transcript stayed intact.
- Prune removed the one empty QA root, leaving populated parent/child sessions.
  No user's original workspace/session was edited, trusted, run or pruned.
- Fresh installation and upgrade qualification passed using disposable fixtures;
  the installed release used its bundled engine.
