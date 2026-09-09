# AGENTS.md

## Project

QQ is a local-first Rust toolkit for building, running, and orchestrating many AI
agents. It ships as one `qq` binary for interactive terminal use, direct
automation, and a long-running HTTP/SSE server. Speed and developer experience
are product requirements; correctness, durability, and safe execution are
baseline constraints.

Read `docs/design/architecture.md` before changing system boundaries. It
records the initial direction, not a license to pre-build deferred features.

Documentation is organized in `docs/README.md`: `design/` for the system as
built, `adr/` for decisions, `plans/` for what is next, `plans/progress/` for
what is in flight, `runbooks/` for procedures. Before starting or reviewing a
unit of work, read `docs/plans/workflow.md`; it defines slices, ledgers,
review, and escalation. Record progress in the plan's ledger every session and
reserve ADR numbers in `docs/plans/progress/root.md`.

## Repository Map

- `src/`: binary, CLI, runtime composition, and authenticated model discovery.
- `crates/qq-auth/`: provider OAuth flows and credential storage.
- `crates/qq-client/`: HTTP/SSE client, reconnect/replay, and TUI client port.
- `crates/qq-config/`: layered configuration, policy, and provider presets.
- `crates/qq-core/`: agent runtime, sessions, tools, and persistence behavior.
- `crates/qq-mcp/`: MCP client transport and tool discovery.
- `crates/qq-provider/`: provider-neutral model API and provider adapters.
- `crates/qq-protocol/`: shared commands, events, identifiers, and wire types.
- `crates/qq-reasoning/`: shared reasoning event vocabulary.
- `crates/qq-server/`: HTTP/SSE server and local-instance discovery.
- `crates/qq-tui/`: terminal UI and client-side state.
- `xtask/`: repository automation; invoke it with `cargo xtask`.

Keep dependencies pointed toward the narrow protocol and provider interfaces.
The root package is the composition root and translates external configuration
into crate-specific settings.

## Developer Workflow

The pinned stable Rust toolchain includes `rustfmt` and Clippy. A Nix development
shell is also available.

```sh
nix develop
cargo run -- ask "Reply with pong"
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace
cargo test -p qq-provider --no-default-features --features test-support
```

The last line covers the minimal embedding profile (no Amazon Bedrock family,
no AWS SDK closure); run it when touching `qq-provider` manifests, features,
or anything under `aws.rs`, `providers/bedrock.rs`, or `providers/mantle.rs`.

Run the narrowest useful test while iterating, then run the workspace checks
before opening a PR. Run
`cargo bench -p qq-provider --bench provider_compiler` when changing provider
compilation or its hot path.

Never commit secrets, local credentials, `target/`, or generated build output.

## Engineering Priorities

1. Optimize for fast, responsive orchestration of many concurrent agents. No
   avoidable lag in startup, time to first token, streaming, tools, persistence,
   replay, or rendering.
2. Prefer the simplest design that is fast and easy to use and maintain. Do not
   trade developer speed for speculative flexibility or complex features.
3. Preserve correctness and durable state. Persist authoritative events before
   publishing them; make retries idempotent where work may be repeated.
4. Keep resource use predictable. Bound tasks, queues, channels, caches, output,
   and concurrency; apply backpressure and cancellation.
5. Measure meaningful hot paths. Support performance complexity with benchmarks
   and optimize end-to-end latency rather than isolated microbenchmarks.

Do not introduce placeholder crates, framework layers, generic extension points,
or alternate protocols without a concrete implemented need. Add dependencies
only for behavior being shipped.

## Rust Style

- Write safe, stable, idiomatic Rust and retain `#![forbid(unsafe_code)]`.
- Keep logic in one function or method unless an extracted unit is genuinely
  reusable, composable, or creates a meaningful interface. Do not create helpers
  merely to make a function shorter.
- Do not reach for `?` by default. Handle expected failures explicitly. When
  propagation is the correct behavior, preserve context and map the failure into
  a specific error type rather than erasing it.
- Prefer domain-specific error enums, normally derived with `thiserror`. Make
  error variants actionable and preserve sources. Avoid stringly typed errors
  and broad `Box<dyn Error>` in library interfaces.
- Use structs and enums to encode invariants and impossible states. Keep public
  interfaces small and choose ownership deliberately.
- Avoid unnecessary allocation, cloning, boxing, dynamic dispatch, and data
  conversion, especially in streaming and request hot paths. Share immutable
  data when ownership is truly shared.
- Prefer exhaustive `match` handling over silent fallbacks. Never discard an
  error without an explicit reason.
- Keep blocking work off Tokio executor threads. Use bounded blocking work when
  unavoidable, and ensure long-running async work supports cancellation.
- Format with rustfmt and fix Clippy findings rather than suppressing them unless
  the lint is demonstrably inappropriate.
- Add concise comments only for non-obvious invariants, safety constraints, or
  design decisions. Do not narrate the code.

### Modules

Never create `mod.rs` files. Use a sibling module file plus a directory:

```text
src/providers.rs
src/providers/openai.rs
src/providers/anthropic.rs
```

Declare children from `providers.rs`. Prefer this layout over
`src/providers/mod.rs` plus `src/providers/*.rs`.

## Architecture Constraints

- Reuse the same core runtime across TUI, server, and direct CLI paths; do not
  create parallel agent implementations.
- Keep provider protocol, authorization, transport, framing, and retry details
  inside `qq-provider`. Provider identity must not branch in request hot paths.
- Keep `qq-protocol` transport-neutral and version externally visible wire data.
- Do not leak application configuration types throughout the workspace.
- Keep queues bounded and session-aware. Never hold a synchronous lock across an
  `.await` or perform blocking I/O directly on a Tokio worker.
- Treat persisted session history as authoritative. A failed write must never be
  presented as durable output.
- Keep tool paths within the selected workspace and require explicit approval
  for destructive or externally visible actions.

## Tests

- Add a regression test for every bug fix and focused tests for new behavior.
- Test public behavior and failure modes, not private implementation trivia.
- Keep tests deterministic; avoid live network services and real credentials.
- Include concurrency, cancellation, bounds, and replay/idempotency cases when a
  change touches those guarantees.
- Benchmark changes that may affect startup, time to first token, streaming,
  provider compilation, persistence, or rendering latency.

## Git And Reviews

Linear is the source of work tracking:

- Team: `DEV`
- Project: `qq`

Use the Executor MCP/tool integrations as the primary interface for both Linear
and GitHub. Read the issue before implementation, link the PR to it, and keep its
state current. Do not guess issue or PR details when they can be queried.

Branch names must start with a Conventional Commit type followed by `/` and a
short kebab-case description. Include the Linear identifier when available:

```text
feat/dev-123-provider-cache
fix/dev-456-sse-reconnect
docs/agent-guide
```

Commit messages and PR titles use Conventional Commits:

```text
type(scope): imperative summary
feat(provider): add compiled provider cache
fix(runtime): preserve session context
feat(protocol)!: replace the event envelope
```

Valid types include `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `build`,
`ci`, `chore`, `style`, and `revert`. Use a focused scope such as `runtime`,
`provider`, `protocol`, `tui`, `cli`, or `config`. Add `!` before `:` for a
dangerous or potentially breaking change and explain the impact in the commit or
PR body.

Keep commits focused and reviewable. PRs must explain the user-visible behavior,
important design decisions, verification performed, and performance or
compatibility impact.

## Concurrent Agent Work

- Inspect the worktree before editing and never overwrite or revert unrelated
  changes made by another agent or developer.
- Parallelize independent read-only investigation. Do not let multiple writing
  agents edit the same checkout concurrently; use isolated worktrees or
  sandboxes and integrate reviewed patches.
- Keep changes narrow. Avoid drive-by refactors, broad formatting churn, and
  edits outside the issue's scope.
- Prefer fast feedback loops and non-interactive commands. Report blockers and
  failed verification directly.

## Definition Of Done

A change is complete when its behavior and failure paths are tested, workspace
formatting and lint checks pass, relevant documentation is current, hot-path
impact is measured when applicable, and the linked Linear issue and GitHub PR
accurately describe the result.
