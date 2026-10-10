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
cd src-tauri
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

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
