# Runbook: local development

## Toolchain

`rust-toolchain.toml` pins the channel (currently `1.97.1`, minimal profile,
plus the `x86_64-unknown-linux-musl` target). `rustup` resolves it on the first
`cargo` invocation. A Nix development shell (`flake.nix`) provides the same
toolchain plus `rustfmt` and Clippy:

```sh
nix develop
```

## Gates

Run the narrowest useful test while iterating, then the workspace gates before
every push. These are the same checks CI runs.

```sh
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace
cargo test -p qq-provider --no-default-features --features test-support   # when qq-provider changed
```

Run `cargo bench -p qq-provider --bench provider_compiler` when provider
compilation changes, and the focused fixtures in
[`perf-recording.md`](./perf-recording.md) when a slice names a gate.

## `qq --version`

`build.rs` embeds the checked-out short SHA and commit date, with a `-dirty`
suffix when the tree has uncommitted changes, so a binary can always be traced
to its source. Cargo reruns the script when `HEAD` moves. See
[`release.md`](./release.md) for how releases pin this value.

## Test environment notes

- Workspace tests bind loopback listeners for the HTTP/SSE server and the
  fake provider; a host that blocks loopback will fail them.
- Some color-output tests fail if `NO_COLOR` is set in the environment. Unset
  it for the test run: `env -u NO_COLOR cargo test --workspace`.
- `cargo test` count at the time of writing is about 1,220 tests; a slice's
  ledger receipt records the exact passed/ignored counts.
- Tests create temporary SQLite stores and workspaces; nothing touches the
  user's XDG state.

## Smoke

```sh
cargo run -- ask "Reply with pong"      # needs a configured model
cargo run -- config check               # validates configuration; a model is not required (it is checked at run time)
```

## Concurrent agents

Use one worktree per writing agent:

```sh
git worktree add ../qq-<slice> -b <type>/<linear-id>-<slice>-<short> origin/main
```

Each worktree builds into its own `target/`. Perf recordings go under that
worktree's `target/qq-perf/` and are never committed. Remove with
`git worktree remove ../qq-<slice>` when the slice merges.

## Never commit

Secrets, local credentials, `.qq/config.d/*-local.ron`, `target/`, perf
reports, or generated build output.
