# Runbook: cutting a release

A release is a `vX.Y.Z` tag on `main`. Pushing the tag runs
`.github/workflows/release.yml`, which builds `qq` for every supported target,
checks `qq --version`, and publishes a GitHub release with the archives, a
combined `SHA256SUMS`, and notes generated from the merged PR titles.

## Procedure

1. Be on an up-to-date, clean `main` whose CI is green.

   ```sh
   git switch main && git pull --ff-only
   git status --short   # must print nothing
   ```

2. Bump, commit, and tag in one step. The version must be plain
   `MAJOR.MINOR.PATCH` and greater than the current one.

   ```sh
   cargo xtask release 0.2.0
   ```

   This rewrites `[workspace.package] version` in `Cargo.toml` (every crate
   inherits it), refreshes `Cargo.lock`, commits `chore(release): v0.2.0`,
   and creates the annotated tag `v0.2.0`. It refuses a dirty worktree or an
   existing tag. Use `--no-commit` to inspect the bump first.

3. Push the commit and the tag together.

   ```sh
   git push origin main --follow-tags
   ```

4. Watch the `Release` workflow. The first job fails fast if the tag does not
   match the manifest version, so a hand-made tag on the wrong commit never
   produces a release. The publish job runs only after every target builds.

## Versioning

Keep the `vX.Y.Z` tag, the manifest, and `qq --version` in agreement; the
workflow enforces the first two, and `build.rs` derives the third. Choosing
the number: bump `MINOR` when a plan phase or a user-visible feature lands,
`PATCH` for fixes only, `MAJOR` when `PROTOCOL_VERSION` or the store schema
changes incompatibly (see `docs/design/architecture.md`).

## What `qq --version` prints

```text
qq 0.2.0 (151fe94 2026-09-09)
```

`build.rs` embeds the short SHA and commit date at compile time, adding
`-dirty` when the worktree had uncommitted changes. The workflow exports
`QQ_GIT_SHA` / `QQ_GIT_DATE` explicitly so the value is independent of the
checkout's git state; a source tarball without `.git` and without those
variables prints `unknown` for both. The build never fails on revision lookup.

## Targets

| Target | Runner | Archive |
| --- | --- | --- |
| `x86_64-unknown-linux-musl` | `ubuntu-latest` | `.tar.gz` |
| `aarch64-unknown-linux-musl` | `ubuntu-24.04-arm` | `.tar.gz` |
| `aarch64-apple-darwin` | `macos-15` | `.tar.gz` |
| `x86_64-apple-darwin` | `macos-15-intel` | `.tar.gz` |
| `x86_64-pc-windows-msvc` | `windows-latest` | `.zip` |

Every target builds natively on its own runner because `aws-lc-sys` compiles
C and cross-linking it is fragile. Linux builds are static musl binaries that
run on any distribution. All of these runners are free for public repositories
at the time of writing; on a private plan the `ubuntu-24.04-arm` and
`macos-15-intel` rows may need to be dropped or paid for.

## Recovering from a bad release

- **Tag pushed, workflow failed.** Fix on `main`, then cut the next patch
  version; do not move or reuse a tag that has been pushed.
- **Wrong version committed locally, not pushed.** `git reset --hard HEAD~1
  && git tag -d vX.Y.Z`, then rerun `cargo xtask release`.
- **Release published with a bad binary.** Mark it as a pre-release or delete
  it in the GitHub UI, then release the fix as the next patch version.
