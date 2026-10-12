# Runbook: local development

## Toolchain

`rust-toolchain.toml` pins the channel (currently `1.97.1`, minimal profile)
with the `clippy`, `llvm-tools-preview`, `rust-analyzer`, `rust-src`, and
`rustfmt` components and two extra targets: `x86_64-unknown-linux-musl` (the
static evaluation binary) and `wasm32-unknown-unknown` (`qq-client` for
browsers). `rustup` resolves it on the first `cargo` invocation. A Nix
development shell (`flake.nix`) builds the same toolchain from that file:

```sh
nix develop
```

## Gates

The feedback loop, the merge gate, and how to read failures are in
[`testing.md`](./testing.md). In short:

```sh
cargo xtask check          # affected packages: fmt, lint, clippy, nextest
cargo xtask smoke          # the real qq binary against a loopback fake provider
cargo xtask check --full   # what the merge queue runs
```

Run `cargo bench -p qq-provider --bench provider_compiler` when provider
compilation changes, and the focused fixtures in
[`perf-recording.md`](./perf-recording.md) when a slice names a gate.

## `qq --version`

`build.rs` embeds the checked-out short SHA and commit date, with a `-dirty`
suffix when the tree has uncommitted changes, so a binary can always be traced
to its source. Cargo reruns the script when `HEAD` moves. Release builds (and
`QQ_EXACT_REVISION=1`) also rerun it when any tracked file changes, so their
`-dirty` flag is exact. A debug build's flag can lag until the next commit,
because watching every file would relink `qq` on every edit. See
[`release.md`](./release.md) for how releases pin this value.

## Test environment notes

- Workspace tests bind loopback listeners for the HTTP/SSE server and the
  fake provider; a host that blocks loopback will fail them.
- Some color-output tests fail if `NO_COLOR` is set in the environment.
  `cargo xtask check` unsets it for the test gate; unset it yourself for a
  direct run: `env -u NO_COLOR cargo nextest run -p qq-tui`.
- Tests create temporary SQLite stores and workspaces; nothing touches the
  user's XDG state.

## Smoke

`cargo xtask smoke` needs no model or credentials. Against a real provider:

```sh
cargo run -- ask "Reply with pong"      # needs a configured model
cargo run -- config check               # validates configuration; a model is not required (it is checked at run time)
```

User-global `~/.config/qq/config.ron` may be a Home Manager (or similar) leaf
symlink to a regular file. Project `.qq/config.ron` still cannot be a symlink.
`QQ_CONFIG_CONTENT` remains the inline supervisor path.

## Concurrent agents

Use one worktree per writing agent:

```sh
git worktree add ../qq-<slice> -b <type>/<linear-id>-<slice>-<short> origin/main
```

Each worktree builds into its own `target/` by default. Perf recordings go
under that worktree's `target/qq-perf/` and are never committed. Remove with
`git worktree remove ../qq-<slice>` when the slice merges.

With several worktrees (for example under `.worktrees/`), a full workspace
build per checkout costs tens of gigabytes. Share one build cache instead by
exporting, in each worktree's shell,

```sh
export CARGO_TARGET_DIR=<main-checkout>/target
```

so every checkout reuses the same compiled dependencies. Concurrent builds
then serialize on the target directory's lock (cargo waits for the other
build to finish rather than corrupting it), and a checkout on a different
commit recompiles only the workspace crates that differ. This is a personal
setting: the repository ships no `.cargo/config.toml` for it because that
would also redirect CI caches, and the Nix development shell does not set it.

## Never commit

Secrets, local credentials, `.qq/config.d/*-local.ron`, `target/`, perf
reports, or generated build output.
