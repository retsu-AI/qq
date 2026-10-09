# xtask

Repository automation for QQ is available through `cargo xtask`.

- `cargo xtask providers check offline` runs the deterministic provider gate.
- `cargo xtask providers check live ...` runs explicitly enabled provider
  canaries.
- `cargo xtask eval run ...` builds a revision-stamped QQ binary (optionally
  for a static `--target`), selects the in-container `--approval` policy, and
  launches the pinned Harbor adapter.
- `cargo xtask eval classify ...` records one trajectory-grounded failure
  category.
- `cargo xtask eval report ...` verifies fixed trial identity and emits the
  baseline scorecard.
- `cargo xtask eval compare ...` pairs two compatible jobs task by task with a
  bootstrap confidence interval on the pass-rate difference.
- `cargo xtask perf baseline ...` records the optimized Linux Phase 0 size,
  startup, RSS, runtime, replay, tool, streaming, and isolated load profiles.
- `cargo xtask perf check ...` rejects compatible candidate reports that exceed
  the checked-in regression budgets.
  (`perf` also has hidden `load-worker`, `r4-worker`, and `feed-worker`
  subcommands that `baseline` spawns as isolated processes. They run only as
  an optimized build on Linux, so invoke them directly through
  `cargo run --release --package xtask -- perf <worker> ...`, not the debug
  `cargo xtask` alias; `docs/runbooks/perf-recording.md` lists the focused
  cases.)
- `cargo xtask release X.Y.Z` bumps the workspace version, writes the
  `CHANGELOG.md` section, and commits it for a PR (`--no-commit` writes the
  files without committing); `cargo xtask release --tag` tags the merged
  `main`; `cargo xtask release --docs` moves the guide's and README's version
  pins once the release's assets are published (`docs/runbooks/release.md`).
- `cargo xtask homebrew-formula --version X.Y.Z --sums SHA256SUMS` renders the
  Homebrew tap formula from a release's checksum file
  (`docs/runbooks/release.md` § Homebrew tap).

See `benchmarks/harbor/README.md` for the reproducible evaluation workflow and
`benchmarks/perf/README.md` for the performance protocol and metric inventory.

```sh
cargo xtask providers check offline
QQ_LIVE_PROVIDER_TESTS=1 cargo xtask providers check live --provider google
QQ_LIVE_PROVIDER_TESTS=1 cargo xtask providers check live --all
```

The live command uses the checked-in matrix in `src/providers.rs`, emits one
redacted JSON record per case, and exits nonzero when a selected case fails or
has no credential. See `docs/design/providers.md` for credential, cadence, and
result-record policy. AWS-specific overrides are `QQ_CANARY_AWS_REGION`,
`QQ_CANARY_AWS_PROFILE`, `QQ_CANARY_BEDROCK_API_KEY`, and
`QQ_CANARY_MANTLE_API_KEY`.
