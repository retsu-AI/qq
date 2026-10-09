---
description: Load before writing or reviewing Rust: functional design, ownership and borrowing, typed errors, bounded async, performance, and verification, with task-specific reference chapters.
---

# Rust Quality

Use for Rust implementation, refactoring, debugging, performance work, and code
review. Not for prose-only edits. Scoped `AGENTS.md` instructions take precedence;
this skill supplies engineering methods, not tool permissions or new policy.

## Load the relevant chapters before working

The table is a routing procedure, not optional further reading. Read the selected
files with `read_file` using their workspace paths below. Markdown links remain
relative to this skill file; the read paths are workspace-root paths.

| Selected chapter | `read_file` workspace path | Markdown link |
| --- | --- | --- |
| Functional design | `.qq/skills/rust-quality/references/functional-design.md` | [Functional design](references/functional-design.md) |
| Async and concurrency | `.qq/skills/rust-quality/references/async-and-concurrency.md` | [Async and concurrency](references/async-and-concurrency.md) |

Batch independent reads.
Do not assume `load_skill` automatically loads supporting files. Load additional
chapters when investigation reveals a new concern; do not load everything for a
small unrelated edit.

| Task or symptom | Required references under `.qq/skills/rust-quality/` |
| --- | --- |
| New behavior or refactor | [Functional design](references/functional-design.md), [Ownership](references/ownership.md), [Testing](references/testing.md) |
| Borrow/lifetime/clone problem | [Ownership](references/ownership.md) |
| Public API, domain states, traits, generics | [Types and APIs](references/types-and-apis.md), [Ownership](references/ownership.md) |
| Fallible operation, retry, panic, error conversion | [Error handling](references/error-handling.md) |
| Task, channel, stream, lock, timeout, shutdown | [Async and concurrency](references/async-and-concurrency.md), [Error handling](references/error-handling.md), [Testing](references/testing.md) |
| Persistence, replay, authoritative state | [Functional design](references/functional-design.md), [Async and concurrency](references/async-and-concurrency.md), [Testing](references/testing.md) |
| Startup, first token, streaming, allocations, latency | [Performance](references/performance.md), plus the chapter for the affected mechanism |
| Lint/build/feature failure or finishing a Rust change | [Tooling and verification](references/tooling-and-verification.md) and `load_skill("qq-verify")` |
| Review-only | [Review procedure](references/review.md), then the chapters implicated by the diff |

Every chapter contains decision rules, worked examples, pitfalls, and tests or
review checks. Rust blocks are complete examples unless marked **fragment** or
**counterexample**; imports describe required existing dependencies, not a request
to add them. They are patterns, not drop-in replacements for QQ's existing APIs.

## Working procedure

1. **Inspect:** worktree, scoped instructions, owning plan/ledger, implementation,
   callers, tests, manifest/features, pinned toolchain. Read architecture before
   boundary changes. Keep existing behavior and unrelated work intact.
2. **Specify:** state observable success, invalid input, failure/cancellation
   behavior, compatibility, bounds, and performance metric where relevant.
3. **Design:** choose the smallest meaningful functional/effect boundary. Identify
   owners, valid states, error policy, and every task/queue's bound and lifecycle.
   Use types and borrowing to eliminate concrete bugs, not to invent a framework.
4. **Implement test-first:** reproduce the bug or add focused behavior tests, then
   implement the minimal change. Revisit chapter checklists as mechanisms change.
5. **Verify:** follow `qq-verify`, using the tooling chapter to diagnose failures
   and cover feature/doc/benchmark gaps. Measure hot paths before and after.
6. **Review and report:** inspect the resulting diff using the review chapter.
   State exact verification, measured results, skipped gates, and remaining risks.

## Non-negotiable design priorities

Functional core, explicit effects; local mutation is fine under exclusive
ownership. Ownership transfer beats reflexive cloning; sharing must have a reason.
Types encode invariants; compiler success does not prove business correctness.
Expected failures get explicit policy; propagation preserves typed context.
Async work has an owner, admission bound, cancellation contract, and shutdown path.
Durable output comes from authoritative persisted history, never an optimistic
write assumption. Performance claims require comparable measurements.

## Provenance and maintenance

[Sources and adaptations](references/sources.md) records the reference libraries
examined, their organization, and the choices deliberately changed for QQ. This
is original guidance, not a vendored copy. Keep chapter links and worked examples
valid; validate examples when editing them. A skill cannot enforce compliance by
itself: compiler, tests, lint/CI gates, and evidence-based review remain necessary.
