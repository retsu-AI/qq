# Contributing to QQ

Thanks for helping. This page is the short version for humans; the full
contract that both people and coding agents follow is
[`AGENTS.md`](AGENTS.md), and the process for planned work is
[`docs/plans/workflow.md`](docs/plans/workflow.md).

## Before you start

- **Bugs and small fixes**: open an issue (or find the existing one), then a
  PR. Include a regression test.
- **Features and behavior changes**: open an issue first and describe the
  user-visible behavior. Larger work is planned in `docs/plans/`; check
  [`docs/plans/README.md`](docs/plans/README.md) § Priority for what is
  already scheduled, and the plan's ledger in `docs/plans/progress/` for
  what is in flight, so two people do not build the same thing.
- **Docs**: PRs welcome without an issue. User docs live in
  [`docs/guide/`](docs/guide/); they are amended in the same PR as the
  behavior they describe.

## Setting up

```sh
git clone https://github.com/retsu-AI/qq && cd qq
nix develop            # optional: exact toolchain; otherwise rustup reads rust-toolchain.toml
cargo build --workspace
cargo run -- --version
```

To run QQ against your own credentials while developing, put your model and
providers in `.qq/config.d/50-local.ron` (gitignored) rather than the tracked
`.qq/config.ron`; anything referencing `Stored(...)` is personal to your
machine.

## The checks a PR must pass

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo test -p qq-provider --no-default-features --features test-support   # if you touched qq-provider
```

CI runs the same plus a Windows job and exact-test coverage guards. Run the
narrowest test while iterating; run everything before pushing. Do not push
red.

## Style, briefly

- Safe, idiomatic Rust; `#![forbid(unsafe_code)]` stays.
- Typed errors (`thiserror`) with actionable messages. A message a user can
  hit names the file or credential and the command that fixes it.
- Bounded everything: queues, channels, tasks, output, concurrency.
- No blocking on Tokio workers; no lock held across `.await`.
- No `mod.rs`; children are declared from a sibling file.
- Tests assert behavior and failure modes, deterministically, without live
  services.
- Comments for non-obvious invariants only.

The full list, with reasons, is in `AGENTS.md`.

## Branches, commits, PRs

- Branch: `<type>/<short-kebab>` or `<type>/<linear-id>-<short-kebab>`
  (`fix/eng-859-first-run-config-ux`).
- Commits and PR titles: [Conventional Commits](https://www.conventionalcommits.org)
  — `fix(config): explain how to set a model when none is configured`.
  Types: `feat fix perf refactor test docs build ci chore style revert`;
  scopes: `runtime provider protocol tui cli config core server auth mcp`.
  Add `!` for a breaking change and explain it in the body.
- PR body: what changed for users, design decisions worth knowing, exactly
  how you verified it, performance or compatibility impact. The template
  asks for each.
- Keep PRs focused. Drive-by refactors and formatting churn make review
  slower for everyone.

## Compatibility

`PROTOCOL_VERSION`, `DESCRIPTOR_VERSION`, and the store schema are
externally visible. Changing a wire type means bumping the version, adding
golden fixtures under `crates/qq-protocol/tests/fixtures/`, and an ADR when
the meaning changes. If unsure, ask in the issue first.

## Decisions

Anything a future contributor would otherwise re-argue gets an ADR in
`docs/adr/` (template there). Reserve the number in
`docs/plans/progress/root.md` before opening the PR.

## Reporting security issues

Privately, per [`SECURITY.md`](SECURITY.md). Not in a public issue.

## License

By contributing you agree your contributions are licensed under the
[MIT License](LICENSE).
