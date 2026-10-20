# Contributing to QQ

[`AGENTS.md`](AGENTS.md) is the repository contract for people and agents.
[`docs/plans/README.md`](docs/plans/README.md) contains the current work.
Start with the requested outcome and check for an existing issue or change.
No plan ledger, ADR reservation, or progress document is required.

## Set up

```sh
git clone https://github.com/retsu-AI/qq
cd qq
nix develop            # optional; otherwise rustup reads rust-toolchain.toml
cargo build --workspace
cargo run -- --version
```

Put personal model and provider configuration in the gitignored
`.qq/config.d/50-local.ron`. Do not commit credentials.

Read `git status` before editing. Use an isolated worktree when another agent
or contributor is active, and preserve their changes.

## Change and verify

Reproduce a runtime defect with a narrow behavioral test that fails before
the fix. Make the smallest change that addresses the cause. Prefer deleting
obsolete code to adding abstractions. Documentation and read-only work do
not need artificial runtime tests.

```sh
cargo nextest run -p <crate> -E 'test(/<pattern>/)'   # while editing
cargo xtask check                                  # final gate, unless the push hook runs it
```

Exercise changed CLI, TUI, or server behavior directly or through an existing
smoke scenario or `verify-qq` drive. Review the final diff and commit before
the final gate so its evidence identifies that commit. If the pre-push hook
is enabled, let it run that gate during push; otherwise run
`cargo xtask check` once before pushing. If it fails, read
`target/qq-check/last.json` and use its targeted rerun command while fixing
the cause. Do not repeat the full gate to recover output or weaken tests
to make it pass.

Perf, release builds, mutation testing, and soak runs belong only to tasks
that need them. Report budget overruns in the PR for the user to decide.
Update `docs/guide/` or `docs/design/` when the change makes them wrong.
Wire changes must follow the version and fixture requirements in `AGENTS.md`.
Write an ADR only when requested or when a decision is genuinely contested.

## Submit

Use branches named `<type>/<eng-id>-<short>` when there is an issue, or
`<type>/<short>` otherwise. Use Conventional Commit titles, for example
`fix(core): preserve the pending user request`.

Keep the PR focused. Describe what changed for users, how you verified it,
and any known regression or budget overrun. Link the existing Linear issue
when there is one. When review is requested, address valid findings in new
commits and explain disagreements with evidence.

Agents push, open PRs, and merge only when the user requests those actions.
Honor the user's review boundary, including a Codex 👍 on the final
non-draft PR head when required.

Report security issues privately as described in [`SECURITY.md`](SECURITY.md).
Contributions are licensed under the [MIT License](LICENSE).
