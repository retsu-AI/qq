# Desktop production readiness

Status: implemented and verified locally, including installed release and recovered-store end-to-end qualification (2026-10-17). Signing and hosted CI remain external release gates.
Ledger: [receipts](progress/desktop-production-readiness.md).
Constraints: ADR-0065, the shared native reducer and local server, separate
desktop Cargo workspace, no push or PR mutations. Preserve the pre-existing
AGENTS.md and capabilities/default.json edits. No reference code is copied.

## Comparative analysis

Source inspection on 2026-10-17 after `git pull --ff-only` reported all five
clones current. Paths below are relative to `../refs/<column>/`; QQ paths
are relative to this checkout. P = present, L = partial, A = absent in the
inspected surface. Terminal features do not establish a desktop capability.
Codex revision ce0323f2; Goose bfcc6ee; OpenCode 8c173a8;
Gemini ed96faed; Crush 12f96b4a.

| Feature | QQ desktop at baseline | QQ TUI | Codex | Goose | OpenCode | Gemini CLI | Crush (ideas only) |
| --- | --- | --- | --- | --- | --- | --- | --- |
| Sessions/search | L: `apps/desktop/src/components/Sidebar.tsx`, no search | P: `crates/qq-tui/src/app/pickers.rs` | P: `codex-rs/tui/src/resume_picker.rs` | P: `ui/desktop/src/components/sessions/SessionListView.tsx` | P: `packages/app/src/home/sessions/controller.tsx` | P: `packages/cli/src/ui/components/SessionBrowser.tsx` | P: `internal/ui/dialog/sessions.go` |
| Rename/archive/delete | L: delete only, `Sidebar.tsx` | L: delete/prune, `crates/qq-tui/src/app.rs` | P: `codex-rs/tui/src/session_archive_commands.rs`, `chatwidget.rs` | L: rename/delete, `ui/desktop/src/components/SessionActionsHeader.tsx`; archive metadata only | L: `packages/app/src/home/sessions/controller.tsx`; archive explicitly unavailable | L: delete, `packages/cli/src/ui/hooks/useSessionBrowser.ts` | L: rename/delete, `internal/ui/dialog/sessions.go` |
| Cancel/steer/queue | L: cancel/queue, `apps/desktop/src-tauri/src/project.rs` | P: `crates/qq-tui/src/app.rs` | P: `codex-rs/tui/src/chatwidget/input_queue.rs` | P: `ui/desktop/src/components/ChatInput.tsx` | P: `packages/app/src/session/composer/queue.ts` | L: cancel, `packages/cli/src/ui/hooks/useGeminiStream.ts` | L: cancel/queue, `internal/ui/model/ui.go` |
| Model/effort/profile | L: model only, `apps/desktop/src/components/Composer.tsx` | P: `crates/qq-tui/src/app/pickers.rs` | P: `codex-rs/tui/src/chatwidget/model_popups.rs` (model/effort) | L: `ui/desktop/src/components/settings/models/` | P: `packages/app/src/providers/models/selection.tsx`, `agent.ts`, `variant.ts` | L: model, `packages/cli/src/ui/components/ModelDialog.tsx` | P: model/effort, `internal/ui/dialog/models.go`, `reasoning.go` |
| File context | A | P: `crates/qq-tui/src/input.rs`, `app.rs` | P: `codex-rs/tui/src/bottom_pane/chat_composer.rs` | P: `ui/desktop/src/components/MentionPopover.tsx` | P: `packages/app/src/composer/prompt-parts.ts` | P: `packages/cli/src/ui/hooks/atCommandProcessor.ts` | P: `internal/ui/dialog/filepicker.go` |
| Approvals/questions | P: `apps/desktop/src/components/ApprovalCard.tsx` | P: `crates/qq-tui/src/app.rs` | P: `codex-rs/tui/src/history_cell/approvals.rs` | P: `ui/desktop/src/components/ToolApprovalButtons.tsx` | P: `packages/app/src/session/requests/session-permission-dock.tsx` | P: `packages/cli/src/ui/hooks/useGeminiStream.ts` | P: `internal/ui/dialog/permissions.go` |
| Command palette | A | P: `crates/qq-tui/src/commands.rs` | P: `codex-rs/tui/src/chatwidget.rs` | P: `ui/desktop/src/components/ChatInput.tsx` | P: `packages/app/src/composer/commands.tsx` | P: `packages/cli/src/ui/commands/mcpCommand.ts` | L: dialog keymaps |
| MCP visibility/management | A | L: capabilities, `crates/qq-tui/src/app.rs` | L: `codex-rs/tui/src/chatwidget/mcp_startup.rs` | P: `ui/desktop/src/components/settings/extensions/ExtensionsSection.tsx` | P: `packages/app/src/settings/providers/extensions.tsx` | P: `packages/cli/src/ui/commands/mcpCommand.ts` | P: `internal/ui/dialog/mcp_toggles.go` |
| Settings/themes | L: OS theme and provider dialog, `apps/desktop/src/styles.css` | P: `crates/qq-tui/src/commands.rs` | P: `codex-rs/tui/src/theme_picker.rs` | P: `ui/desktop/src/components/settings/app/AppSettingsSection.tsx` | P: `packages/app/src/settings/appearance/appearance.tsx` | P: `packages/cli/src/ui/components/SettingsDialog.tsx` | P: `internal/ui/dialog/theme.go` |
| Notifications | A: attention event has no consumer | L: terminal notification | L: terminal OSC/BEL, `codex-rs/tui/src/notifications/` | P: `ui/desktop/src/main.ts` | P: `packages/desktop/src/renderer/platform/notifications.ts` | L: terminal, `packages/cli/src/ui/hooks/useRunEventNotifications.ts` | P: `internal/ui/notification/native.go` |
| Single instance | A | A: independent terminals intentional | A: no native desktop source | P: `ui/desktop/src/main.ts` | P: `packages/desktop/src/main/lifecycle/configure.ts` | A: terminal | A: terminal |
| Large transcript | L: native bounds, all retained rows mounted | P: `crates/qq-tui/src/view/transcript.rs` | P: `codex-rs/tui/src/app/history_pagination.rs` | L: `ui/desktop/src/components/ProgressiveMessageList.tsx` eventually mounts all | P: `packages/app/src/session/timeline/virtualizer.tsx` | P: `packages/cli/src/ui/components/shared/VirtualizedList.tsx` | L: viewport, no bounded transcript proof |
| Recovery/diagnostics | L: reconnect exists; failed startup requires reopening | P: `crates/qq-tui/src/terminal.rs`, `src/doctor.rs` | P: `codex-rs/tui/src/bottom_pane/chat_composer/reconnect.rs` | P: `ui/desktop/src/startupDiagnostics.ts` | P: `packages/app/src/runtime/server/server-sync/connection.ts` | P: `packages/cli/src/ui/hooks/useGeminiStream.ts` | L: workspace refresh |
| Accessibility | L: keyboard shortcuts, incomplete labels/focus | L: terminal keyboard | P: `codex-rs/tui/src/screen_reader.rs` | L: keyboard and alerts | L: labelled controls/keybindings | P: `packages/cli/src/ui/layouts/ScreenReaderAppLayout.tsx` | L: dialog keymaps |
| Auto-update | A | A | P: `codex-rs/tui/src/update_prompt.rs` | P: `ui/desktop/src/utils/autoUpdater.ts` | P: `packages/gui-extensions/src/updater/main.ts` | P: `packages/cli/src/ui/utils/updateCheck.ts` | A: not established |

Baseline verification: one Rust desktop test passes; TypeScript typecheck,
format and Clippy pass. Dependency warnings exist in qq-auth/qq-core on Windows.
No stored credentials or discovery tokens were inspected.

## Prioritized backlog and slices

Each row is independently reviewable; desktop checks from README apply to every
commit. Inputs are preceding slices only where named. Owned paths are desktop
sources/tests plus this plan and ledger, except the explicit shared requests.

| Slice | Priority / placement | Goal and acceptance | Proof |
| --- | --- | --- | --- |
| DP1 | P0 desktop | Bound actor commands and in-flight requests; retain drafts through durable acknowledgement; show rejection; disable disconnected submissions | queue saturation/closure, receipt/failure, draft preservation tests; offline live pass |
| DP2 | P0 desktop, DP1 | Explicit queue/steer/interrupt/stop; clear replay/offline/startup errors with retry; controls reject unavailable active run | typed command mapping and stale-run tests; live reconnect/cancel pass |
| DP3 | P1 desktop | Search sessions; reversible local archive and rename with explicit local scope; child navigation; delete confirmation keyboard reachable | filtering/persistence tests; restart and keyboard pass |
| DP4 | P1 desktop | Profile, supported effort and approval delegate controls; typed workspace-file attachments with server containment; next-run semantics | DTO/command and input bounds tests; live invalid path/no model rejection |
| DP5 | P1 desktop | Settings: system/light/dark, font size, notifications, shortcuts; MCP capability visibility and diagnostics; command palette over implemented actions | preference migration/validation and UI logic tests; focus/keyboard checks |
| DP6 | P1 desktop | Single instance, attention/completion notifications respecting focus and preferences | transition/deduplication tests; second-process and unfocused live checks |
| DP7 | P1 desktop | Bounded transcript mounting with full retained-history access; generated/checked view-model contract | rendering bounds, streaming/focus tests; 512-session/large transcript timing baseline and after |
| DP8 | P1 build/docs | Windows desktop CI; release metadata consistency, signed-release procedure, guide and README | all desktop gates, root gates, release NSIS containing qq.exe, live end-to-end receipt |
| DP9 | P2 desktop | Richer diff navigation, tray, arbitrary key remapping | evaluate after P0/P1; no speculative runtime extensions |

## Decisions

| Choice | Alternatives / reason | Reversal |
| --- | --- | --- |
| Keep protocol 32 and native reducer | Existing commands cover run controls, profiles, effort, file parts; no need for a second state implementation | Engine change requires separate slice, ADR and compatibility evidence |
| Local session aliases/archive when implementing DP3 | Durable cross-client rename/archive needs new wire/storage semantics, absent today; local metadata enables organization without claiming server support | Remove desktop metadata once canonical commands exist; histories untouched |
| Defer binary/image uploads | Only text and contained workspace-file inputs exist; arbitrary uploads require engine/provider design beyond desktop parity | New independently tested protocol slice |
| Defer automatic updates and signing certificate acquisition | No authorized signed feed or signing identity; silent installation would lack a qualified trust chain. Manual installer upgrades and documented signing remain DP8 requirements | Configure signed releases after human supplies certificate and distribution policy |
| MCP management edits remain through existing configuration | Capabilities expose status, but no safe mutation/trust/reload protocol. Display status and explain configuration path; do not fabricate success | Add configuration workflow after separately designed trust-safe mutation API |
| Keep CLI-only developer commands outside desktop palette | Palette lists implemented desktop actions; shell execution of arbitrary CLI commands is not a substitute for typed controls | Add typed commands individually |
| No separate Linear/PR writes | User explicitly prohibits pushing/opening/editing PRs; this local branch and ledger track the work | Human publishes after review |
| Existing-session pickers; configured defaults for new sessions | Existing typed commands change a session. Avoid inventing draft-session configuration or silently overriding project defaults | Add explicit create options when the shared client supports them |
| Defer tray, arbitrary key remapping and richer diff navigation (DP9) | Existing inline diffs, window lifecycle and documented shortcuts cover the current workflow; these P2 additions need separate interaction design and qualification | Independent desktop-only slices |
| Isolate connected qualification from real projects | Use a throwaway project and a NoAuth loopback provider; local test-store handling stays outside git | Repeat the same fixture workflow in a clean environment |
| Native dialog focus isolation for all overlays | Approval keys must not act behind settings/provider/palette dialogs or in editable controls; repeat/composition/modifier events are ignored | Retain equivalent tested focus isolation if dialog implementation changes |
| Generated native DTOs with explicit embedded protocol mappings | ts-rs catches native DTO drift without coupling engine/protocol crates to a desktop generator; embedded mappings require review when protocol vocabulary changes | Extend generation at the protocol boundary in a separately reviewed slice |

## Verification and completion

Desktop: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`,
`cargo test --locked` in src-tauri; `pnpm typecheck` and UI tests in desktop.
Engine changes additionally require root baseline/failure-set comparison and
workspace gates. Never share target directories across checkouts. Record raw
performance artifacts under target/qq-perf, never commit generated evidence.
Live tests use throwaway projects and never read credential/token values.
Final acceptance includes release NSIS, live verification, process cleanup,
small verified local commits, and only the two original uncommitted edits.
