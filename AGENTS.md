# AGENTS.md

QQ is a local-first Rust agent harness: one `qq` binary for the TUI, CLI
automation, and an HTTP/SSE server, all on one core runtime.

## How to work

- **Do the task the user asked for, then stop.** Finish with the result, not
  a status table. If something blocks you, name the blocker in one sentence
  and what you need from the user. Do not invent extra gates, experiments,
  or follow-up work.
- For a non-trivial task, load the `qq-mode` skill; it routes to the
  playbook and the PR skills (`open-pr`, `babysit-pr`, `land-pr`).
- `git status` first. Use your own worktree when another agent may be active:
  `git worktree add .worktrees/<name> -b <type>/<short> origin/main`. Never
  touch another agent's uncommitted changes.
- No plan ledgers, ADR reservations, or progress docs are required. Write an
  ADR only when the user asks for one or a decision is genuinely contested.
- Update `docs/design/` or `docs/guide/` only when your change makes them
  wrong.

## Feedback loop

```sh
cargo nextest run -p <crate> -E 'test(/<pattern>/)'   # while editing
cargo xtask check                                      # once, before pushing
```

Run the narrow test while iterating and `cargo xtask check` once at the
end; do not loop the full gate. Don't run `cargo xtask perf`, release builds,
or mutation testing unless the task is about performance or size. Size and
latency budgets get reported, not blocked on: a budget overrun goes in the
PR description, and the user decides. When `cargo xtask check` fails, read
`target/qq-check/last.json` for the failing test and its rerun command. Fix
the cause; don't weaken the test.

Never `sleep` to wait on a command. Run it to completion with a sufficient
timeout.

## Repository map

- `src/`: binary, CLI, TUI/server wiring, headless driver.
- `crates/qq-core/`: agent runtime, sessions, tools, persistence.
- `crates/qq-provider/`: provider-neutral model API and adapters (retry,
  transport, and auth stay here).
- `crates/qq-protocol/`: wire types; changing them means regenerating
  `crates/qq-protocol/tests/fixtures/` and bumping `PROTOCOL_VERSION`.
- `crates/qq-config/`: layered configuration and policy.
- `crates/qq-client/`, `crates/qq-server/`, `crates/qq-tui/`: client, server,
  terminal UI.
- `crates/qq-host/`: runtime composition (config and credentials to a
  runtime loader, reviewer, model catalog, server handler).
- `crates/qq-auth/`, `crates/qq-mcp/`, `crates/qq-reasoning/`: auth, MCP,
  reasoning events.
- `xtask/`: `check`, `lint`, `smoke`, `perf`, `release`.
- `docs/guide/` (user docs, source of `website/`), `docs/design/` (as built).

## Code rules

- Safe, stable, idiomatic Rust; `#![forbid(unsafe_code)]`; no `mod.rs`.
- Simplest design that works. No speculative abstractions, placeholder
  crates, or new dependencies without a shipped need. Deleting code is
  welcome.
- Bounded channels and queues; no lock held across `.await`; no blocking I/O
  on Tokio workers.
- Persist session events before publishing them.
- No code comments: no `//`, `///`, `//!` or `/* */` in code you write.
  Names, types and tests carry the meaning. A comment explaining a
  workaround means the cause needs fixing instead. Existing comments go
  when the code they describe changes; never add new ones.
- Library errors use `thiserror` enums with sources; no
  `Box<dyn Error>` in library APIs.
- Tests check behavior, deterministically: no network, no real credentials,
  no fixed sleeps. Each bug fix gets a test that fails before the fix.
  Loopback fakes are in `qq_provider::test_support` and `xtask/src/smoke.rs`.

`cargo xtask check` enforces fmt, clippy `-D warnings`, and the
`cargo xtask lint` rules (no `mod.rs`, forbid-unsafe, no unbounded channels,
no new test sleeps, `#[ignore]` reasons, skill copies in `.qq/skills/` and
`.agents/skills/` identical).

## Git

- Branches `<type>/<eng-id>-<short>`; Conventional Commit titles
  (`fix(core): …`). Link the Linear issue (team `ENG`) when there is one.
- PR body: what changed for users, how you verified it, and any known
  regressions or budget overruns. Keep it short.
- Pushing, opening PRs, and merging are only done at the user's request.
  Never commit secrets or `target/`.
