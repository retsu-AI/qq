# AGENTS.md

## Start here

1. For any non-trivial task, load the **`qq-mode`** skill
   (`.qq/skills/qq-mode/SKILL.md`). It picks the playbook, sets your todo
   list, and names the gates that prove the work. This file holds repository
   facts and rules; `qq-mode` holds the procedure.
2. Work in your own worktree when another agent may be active:
   `git worktree add .worktrees/<name> -b <type>/<eng-id>-<short> origin/main`.
   Never edit or revert another agent's uncommitted changes.
3. The feedback loop (details: `docs/runbooks/testing.md`):

```sh
cargo nextest run -p <crate> -E 'test(/<pattern>/)'   # while editing
cargo xtask check                                      # before every push: affected packages only
cargo xtask smoke                                      # real qq binary vs a loopback fake provider
cargo xtask check --evidence                           # Markdown proof for the PR body
```

When a gate fails, read `target/qq-check/last.json` (each failing test, its
panic, and the exact rerun command) or `target/qq-smoke/<scenario>/`
(stdout, stderr, provider requests, session store). Fix the cause; never
weaken the gate. The merge queue runs `cargo xtask check --full`.

## Project

QQ is a local-first Rust toolkit for building, running, and orchestrating
many AI agents. One `qq` binary serves interactive terminal use, direct
automation, and a long-running HTTP/SSE server. Speed and developer
experience are product requirements; correctness, durability, and safe
execution are baseline constraints.

Read `docs/design/architecture.md` before changing system boundaries.
Documentation is organized in `docs/README.md`: `design/` (as built),
`adr/` (decisions), `plans/` (next), `plans/progress/` (in flight, one
ledger per plan), `runbooks/` (procedures). Plan slices follow
`docs/plans/workflow.md`; reserve ADR numbers in
`docs/plans/progress/root.md`.

## Repository map

- `src/`: binary, CLI, runtime composition, authenticated model discovery.
- `crates/qq-auth/`: provider OAuth flows and credential storage.
- `crates/qq-client/`: HTTP/SSE client, reconnect/replay, client port, shared
  session state/reducer (`state`).
- `crates/qq-config/`: layered configuration, policy, provider presets.
- `crates/qq-core/`: agent runtime, sessions, tools, persistence.
- `crates/qq-harness/`: configuration-driven hosting composition, plan cache,
  MCP bridge.
- `crates/qq-mcp/`: MCP client transport and tool discovery.
- `crates/qq-provider/`: provider-neutral model API and adapters.
- `crates/qq-protocol/`: shared commands, events, identifiers, wire types.
- `crates/qq-reasoning/`: shared reasoning event vocabulary.
- `crates/qq-server/`: HTTP/SSE server and local-instance discovery.
- `crates/qq-tui/`: terminal UI and terminal-only client state.
- `xtask/`: repository automation (`cargo xtask check|lint|smoke|perf|eval|release`).
  Releases: `docs/runbooks/release.md`.
- `website/`: docs site generated from `docs/guide/`; edit the guide, not the
  site (`docs/runbooks/website.md`).
- `.qq/skills/`: agent skills (`qq-mode`, `principles`, `verify-qq`), also
  reachable as `.agents/skills/`.

Dependencies point toward the narrow protocol and provider interfaces. The
root package is the composition root and translates external configuration
into crate-specific settings.

## Engineering priorities

1. Fast, responsive orchestration of many concurrent agents. No avoidable lag
   in startup, time to first token, streaming, tools, persistence, replay, or
   rendering.
2. The simplest design that is fast and easy to maintain. No speculative
   flexibility.
3. Correct, durable state. Persist authoritative events before publishing
   them; make retried work idempotent.
4. Predictable resources. Bound tasks, queues, channels, caches, output, and
   concurrency; apply backpressure and cancellation.
5. Measure hot paths end to end; support performance complexity with
   benchmarks.

No placeholder crates, framework layers, generic extension points, or
alternate protocols without a concrete implemented need. Add dependencies
only for behavior being shipped.

## Architecture constraints

- One core runtime behind the TUI, server, and CLI; no parallel agent
  implementations.
- Provider protocol, authorization, transport, framing, and retry stay inside
  `qq-provider`. Provider identity never branches in request hot paths.
- `qq-protocol` is transport-neutral; externally visible wire data is
  versioned (`PROTOCOL_VERSION`, fixtures under
  `crates/qq-protocol/tests/fixtures/`).
- Application configuration types do not leak through the workspace.
- Queues are bounded and session-aware. No synchronous lock across `.await`;
  no blocking I/O on a Tokio worker.
- Persisted session history is authoritative. A failed write is never
  presented as durable output.
- Tool paths stay inside the selected workspace; destructive or externally
  visible actions need explicit approval.

## Rust style

- Safe, stable, idiomatic Rust.
- Keep logic in one function unless the extracted unit is genuinely
  reusable or forms a meaningful interface.
- Handle expected failures explicitly instead of a reflexive `?`. When
  propagating, preserve context in a specific error type.
- Domain error enums with `thiserror`, actionable variants, sources
  preserved. No stringly typed errors or `Box<dyn Error>` in library APIs.
- Encode invariants in types. Small public interfaces; deliberate ownership.
- No unnecessary allocation, cloning, boxing, dynamic dispatch, or
  conversion in streaming and request hot paths.
- Exhaustive `match`; never discard an error without a stated reason.
- Blocking work off Tokio executor threads; long-running async work
  supports cancellation.
- Comments only for non-obvious invariants, safety constraints, or design
  decisions.
- Modules: `src/providers.rs` plus `src/providers/openai.rs`, never
  `src/providers/mod.rs`.

## Rules → enforcers

A rule with an enforcer is checked by a tool; do not rely on remembering
it. When the same correction happens twice, encode it
(`.qq/skills/qq-mode/playbooks/correct.md`) and add a row.

| Rule | Enforced by |
| --- | --- |
| rustfmt formatting | `cargo xtask check` (fmt gate), CI |
| Clippy clean, `-D warnings` | `cargo xtask check` (clippy gate), CI |
| No `mod.rs` files | `cargo xtask lint` `no-mod-rs` |
| `#![forbid(unsafe_code)]` in every crate root | `cargo xtask lint` `forbid-unsafe` |
| No unbounded channels in production code | `cargo xtask lint` `unbounded-channel` |
| No new fixed sleeps in tests (ceiling only moves down) | `cargo xtask lint` `sleep-in-tests` |
| `#[ignore]` carries a reason | `cargo xtask lint` `ignore-needs-reason` |
| Every ADR has an index row | `cargo xtask lint` `adr-indexed` |
| Guide documents every CLI flag and doctor check | `src/docs_truth.rs` tests |
| Wire changes regenerate fixtures and bump `PROTOCOL_VERSION` | `crates/qq-protocol/tests/wire_fixtures.rs` |
| The binary works end to end (ask, run, tools, 401, resume, serve) | `cargo xtask smoke` |
| Startup, first token, size budgets | `cargo xtask perf check` (`benchmarks/perf/budgets-v1.json`) |
| Minimal provider profile builds without the AWS SDK | `cargo xtask check` (minimal-provider gate on AWS paths), merge queue |
| Tests assert behavior, not implementation | prose only: `principles` skill |
| No lock across `.await`, no blocking on Tokio workers | Clippy `await_holding_lock`; otherwise review |
| Persist before publish | review; `crates/qq-core` session tests |

## Tests

- Test public behavior and failure modes at the narrowest boundary that
  proves it. A regression test for every bug fix that nothing existing
  catches; show it failing first.
- Deterministic: no live network, no real credentials, no fixed sleeps, no
  wall-clock upper bounds. Loopback fakes live in
  `qq_provider::test_support` and `xtask/src/smoke.rs`.
- Cover concurrency, cancellation, bounds, and replay/idempotency when a
  change touches them.
- Benchmark changes that may affect startup, time to first token, streaming,
  provider compilation, persistence, or rendering latency.

Policy and tooling: `docs/runbooks/testing.md`.

## Git and reviews

Linear is the work tracker (team `ENG`, project `qq`). Read the issue before
implementing, link the PR to it, keep its state current, and query rather
than guess issue or PR details.

- Branches: `<type>/<eng-id>-<short-kebab>` (`feat/eng-123-provider-cache`).
- Commits and PR titles: Conventional Commits, `type(scope): imperative
  summary`; `!` plus an impact paragraph for breaking changes. Types: `feat`,
  `fix`, `perf`, `refactor`, `test`, `docs`, `build`, `ci`, `chore`,
  `style`, `revert`.
- PRs explain user-visible behavior, design decisions, verification (the
  `cargo xtask check --evidence` block), and performance or compatibility
  impact. Procedure: `.qq/skills/qq-mode/playbooks/opening-a-pr.md`.
- Keep changes narrow: no drive-by refactors or formatting churn outside the
  issue's scope.
- Never commit secrets, credentials, `target/`, or generated output.

## Definition of done

Behavior and failure paths are tested; `cargo xtask check` passes with its
evidence in the PR; user-visible behavior is proven with `cargo xtask smoke`
or `verify-qq`; documentation is current; hot-path impact is measured when
applicable; the Linear issue and PR describe the result.
