# QQ Desktop

A Windows desktop app for QQ: Tauri v2 shell, React + TypeScript + Tailwind UI.

The app is a client of the same user-scoped QQ server the `qq` TUI and CLI use,
so sessions, configuration, and credentials are shared. When no server is
running it starts `qq serve --exit-on-stdin-close` and supervises it.

```
qq serve (engine, unchanged runtime)
   ▲ HTTP + SSE (qq protocol)
src-tauri/  Rust core
   engine.rs   find or start the server
   project.rs  one actor per open project: qq_client::TuiClient + the shared
               qq_client::state reducer; executes UI commands
   view.rs     projects SessionStore into serializable DTOs (≤30/s)
   ▲ Tauri commands + "project-state" events
src/        React UI (sidebar, transcript, approvals, composer)
```

## A separate Cargo workspace

`src-tauri/` is its own Cargo workspace with its own `Cargo.lock` and
`target/`. Tauri links platform webview libraries (WebKitGTK on Linux), and
the engine's workspace, CI, and `cargo build --workspace` must not need them.
The app depends on the engine crates by path, so it always builds against the
engine in this checkout; nothing in the root workspace refers to the app.

## Develop

From the repository root, build the engine once so the app can find it:

```sh
cargo build --bin qq
```

The app looks for its engine in this order: `QQ_DESKTOP_ENGINE`, beside its
own executable (installed builds), the repository's `target/debug/qq.exe`
(debug builds only), then `PATH`.

Then, from a real terminal (`tauri dev` exits when stdin is not a TTY):

```sh
cd apps/desktop
pnpm install
pnpm tauri dev
```

`qq-desktop <folder>` opens that folder as a project at launch.

## Checks

```sh
cd apps/desktop
pnpm typecheck
pnpm test
pnpm check:version
cd src-tauri
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

`src/lib/generated.ts` is generated from the native view DTOs by `ts-rs`.
Rust tests fail when the checked-in contract drifts. Regenerate in PowerShell:

```powershell
cd apps/desktop/src-tauri
$env:QQ_EXPORT_TYPES = '1'
cargo test --locked typescript_view_contract_is_current
Remove-Item Env:QQ_EXPORT_TYPES
```

Protocol vocabulary embedded in DTOs has explicit TypeScript boundary mappings;
review those mappings when changing protocol enums. The root workspace and its
lockfile remain independent of desktop dependencies.

## Desktop controls

- Search sessions by name and recent output. Rename/archive actions are labelled
  **locally**: preferences belong to this desktop, and server history is intact.
  Archived active/needs-you sessions continue to surface; Restore reverses archive.
- During a run choose Queue follow-up, Steer at next boundary, or Interrupt and
  steer. Stop cancels the active run. A draft clears only after server admission;
  uncertain acknowledgement says to check history before retrying.
- Existing sessions expose profile, advertised reasoning effort and approval
  delegate. Profile/effort/model changes apply to the next run; delegate changes
  apply at the next held call. New sessions initially use configured/default values.
- Type `@` for a file picker backed by the TUI's `qq_core::mentions::complete_paths`
  (native `complete_mention` command, bounded blocking pool). The TUI's full `@`
  grammar (ranges, directories, globs, `@diff`, `@web:`, `@sha:`) still applies; the
  shared resolver checks containment, and unresolved mentions show an error and
  retain the draft for correction.
- The window is undecorated: `TitleBar.tsx` draws navigation, the File/Edit/View/Help
  menus and caption buttons; `capabilities/window.json` grants exactly the window
  calls it uses. All pickers use the themed `Dropdown.tsx` instead of `<select>`,
  whose popup Windows draws outside the app theme.
- `tauri-plugin-window-state` saves size, position and maximized state (not
  visibility, so a first launch with nothing saved still shows the window).
- Fenced code is highlighted with the `highlight.js` core and a fixed set of
  bundled languages (`src/lib/highlight.ts`); colors are theme tokens. The
  Changes panel (`ChangesPanel.tsx`) is derived in the UI from the diffs already
  carried on completed edit tool calls (`src/lib/changes.ts`), so it needs no
  native command. `tests/render.html?demo` renders a sample conversation with
  tools, an edit and code for checking these views without an engine.
- Ctrl K opens implemented desktop commands, including child sessions,
  compaction, rollback and empty-session pruning. Ctrl , opens settings,
  shortcuts, connection diagnostics and reported external-tool status.
- Settings select system/light/dark, transcript/composer text size, and optional
  unfocused attention/completion notifications. Windows notifications require an
  installed app. A second process focuses the existing window and forwards folders.
- Transcript rows are dynamically measured and virtualized; Ctrl Home / Ctrl End
  in the history region reach its beginning/end. Upward scrolling pauses following;
  Latest resumes it. The UI exposes the retained native snapshot, not an independent
  transcript database or a new history-paging protocol.

The development-only `tests/render.html?rows=2048&running&reject` fixture renders
512 fake sessions and bounded transcript rows with mocked IPC. It reads no engine
state or credentials and is excluded from the production bundle. Remove `reject`
to exercise accepted submissions; remove `running` for idle controls.

For the acknowledgement-focus regression, open
`tests/render.html?rows=1&running&focus-check`, focus Message, type text and press
Enter. The fixture delays the mock receipt across eight animation frames and
must report **PASS: composer retained focus during and after acknowledgement**.
In the native app, send a streaming request and then press Escape without clicking
back into the composer; the run must cancel. Pending input is read-only while its
receipt is outstanding, preserving focus without permitting duplicate submission.

## Package

```sh
cd apps/desktop
pnpm bundle
```

This builds the release `qq` engine, stages it as a Tauri sidecar
(`src-tauri/binaries/qq-<target-triple>.exe`, ignored by git), and builds a
per-user NSIS installer (no administrator rights) under
`src-tauri/target/release/bundle/nsis/`. The installer puts `qq.exe` beside the app,
which is the first place the app looks for its engine.

The sidecar is declared only in `src-tauri/tauri.bundle.conf.json`, merged in
by `pnpm bundle`: Tauri requires every declared sidecar to exist at compile
time, so keeping it out of `tauri.conf.json` lets `cargo check` and
`pnpm tauri dev` run without building the release engine first.

Packaging first verifies package.json, native Cargo.toml and tauri.conf.json
against the root workspace version. Keep the app identifier `ai.qq.desktop` and
NSIS currentUser install mode stable for in-place upgrades. The Windows workflow
runs frontend/native gates on relevant changes; manual dispatch also builds and
retains the NSIS artifact. It does not publish a release.

## Signing a Windows release

Local installers are unsigned. Distribution signing requires an organization
code-signing certificate accessible to SignTool (and any required hardware token
or signing service), an authorized timestamp service and a protected release host.
Never put certificate material, passwords or service credentials in this repository.

1. Build/stage the engine with `pnpm engine`. Sign the staged
   `src-tauri/binaries/qq-<host>.exe` using the authorized SHA-256 signing identity.
   Verify it with `Get-AuthenticodeSignature` before packaging.
2. Supply a private Tauri configuration overlay with Windows
   `certificateThumbprint`, `digestAlgorithm: "sha256"` and `timestampUrl`, or
   the organization's configured signing command. Run Tauri build with both
   `src-tauri/tauri.bundle.conf.json` and that overlay. This signs the desktop
   executable and installer; do not rerun `pnpm engine` over the signed sidecar.
3. Verify the installer and both installed executables with
   `Get-AuthenticodeSignature`; verify publisher, validity and timestamp, then
   qualify fresh install and upgrade on a disposable Windows account/VM.

See [Tauri Windows signing](https://v2.tauri.app/distribute/sign/windows/).
Automatic updates remain deferred until a signed feed, update-signing identity
and release/rollback policy are supplied. Manual NSIS upgrade is the release path.
