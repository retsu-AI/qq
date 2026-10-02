# apps/ — QQ web surfaces

A separate Cargo workspace (ADR-0018) for everything compiled to
`wasm32-unknown-unknown`: the shell and the remotes it hosts. It depends on
the kernel through `../crates/qq-protocol` and `../crates/qq-client` only.
The root workspace gates do not build this directory; CI runs
`.github/workflows/apps.yml`.

| Crate | Role |
| --- | --- |
| `ui-common` | Framework-neutral glue: protocol compatibility range, authenticated server probe, the remote contract (`RemoteConfig`, `RemoteManifest`). |
| `shell` | The Leptos shell (ADR-0017): remote navigation from `remotes.json`, server list, dynamic loading of remotes, mount/unmount. |
| `sessions` | The first remote. Workspaces, session tree, transcript, composer (U3–U5). |

## Remote contract

A remote is an ES module emitted by wasm-bindgen that exports:

```text
default(init)                                  wasm-bindgen initializer
mount(root: HTMLElement, config: string)       config is RemoteConfig JSON
unmount()
```

The shell lists remotes in `shell/remotes.json` (`RemoteManifest`); `module`
and `wasm` are URLs resolved against the shell's document, so a remote may be
served from any origin that allows it. Any framework can implement the
contract; `qq-ui-common` is offered, not required.

## Build and run

Requires the pinned toolchain (`rust-toolchain.toml` adds the wasm target) and
[Trunk](https://trunkrs.dev) 0.21.

```sh
cd apps
./build.sh            # dist/ = shell, dist/remotes/<name>/ = each remote
./size-gate.sh        # 600 KB gzip per artifact (wasm + JS glue)
(cd dist && python3 -m http.server 8090 --bind 127.0.0.1)
qq serve --allow-origin http://127.0.0.1:8090
```

For a single crate while iterating: `cd shell && trunk serve` (or `cd
sessions && trunk serve` for the remote standalone, without the shell).

## Checks

```sh
cd apps
cargo fmt --all --check
cargo clippy --workspace --target wasm32-unknown-unknown --all-targets -- -D warnings
cargo test --workspace
```
