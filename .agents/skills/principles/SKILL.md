---
name: principles
description: QQ engineering principles for observable behavior, root causes, subtraction, focused tests, and meaningful measurements.
---

# Principles

Use the sections relevant to the task. The user's scope and `AGENTS.md`
govern; reading this reference creates no extra work or reporting gates.

## Prove it works

**When:** before saying a task is done.

Verify against the real artifact: run the feature, read the value, inspect
the diff. Not a proxy ("it compiles"), not a self-report ("tests should
pass"), not a different surface ("the unit test passes" for a CLI bug).

- Exercise changed CLI, TUI, or server behavior directly or through an
  existing smoke scenario or `verify-qq` drive.
- Report the relevant observed result. Read-only work needs no build gate.
- If you could not verify something, say so and say why.

## Fix root causes

**When:** debugging.

Reproduce first. Ask why until the answer is a mechanism you can point at
with `file:line`. Fix it there. Resist guards that hide the symptom: a
`.unwrap_or_default()` over an unexpected `None`, a retry around a race, a
sleep before an assertion. Each of those is a second bug.

## Test behavior, not implementation

**When:** writing, changing, or keeping a test.

Call the code the way its users do (the `SessionRuntime` API, the CLI, the
server routes, the provider trait) and assert what they observe against a
literal expected value. If the test would still pass with the function body
replaced by `Default::default()`, rewrite the assertion or delete the test.

- No fixed sleeps. Wait on the event, channel, or `Notify`; or use
  `#[tokio::test(start_paused = true)]`. `cargo xtask lint` counts sleeps.
- No wall-clock upper bounds in tests; those are benches or perf budgets.
- Prefer a new row in an existing table-driven test over a new function.
- Every runtime bug fix has a test that fails before the fix and passes after.
  Show it failing first.

## Subtract before you add

**When:** sizing a diff, refactoring, or tempted to add an abstraction.

Delete dead code, one-caller wrappers, duplicate tests, and stale
compatibility paths first; then build on the smaller base. The smallest
change that solves the problem wins. No placeholder crates, extension
points, or options without a caller (`AGENTS.md` § Code rules).

## Encode lessons in structure

**When:** you, a reviewer, or the user correct the same thing twice.

Turn the rule into a type, a lint (`xtask/src/lint.rs`, `[workspace.lints]`),
a CI step, or a focused behavioral test when it prevents the actual mistake.
Do not create a separate process or document merely to record a correction.

## Explain the number

**When:** before you trust, report, or act on a measurement.

Find what limits it (CPU, I/O, a lock, allocation, the host's load), how
much it varies run to run (repeat; A/A on one binary), and whether it
measures the path users hit. A 5% win inside 8% noise is not a win. Record
the command, host load, and raw output path.

## Guard the context window

**When:** a search or output is large, or you are about to read many files.

When delegation helps, give a subagent a precise question and owned paths.
Keep conclusions, `file:line` citations, and
decisions in the main thread; not raw output. Read
`target/qq-check/last.json` instead of rerunning a gate to see its output.

## Rust style

**When:** writing Rust here.

`AGENTS.md` § Code rules is the rule. The ones reviewers correct most:
handle expected failures explicitly instead of a reflexive `?`; typed
`thiserror` errors with sources; exhaustive `match` with no silent `_`
fallback; no blocking work or sync lock across `.await` on a Tokio worker;
bounded channels and queues; no `mod.rs` (lint-enforced); no new code
comments of any kind.
