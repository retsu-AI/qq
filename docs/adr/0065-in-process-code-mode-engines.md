# ADR-0065 — Run code-mode scripts in-process, with child calls as durable tool calls

**Status:** Proposed (experimental code mode, PR #558)
**Date:** 2026-11-03
**Deciders:** code-mode experiment
**Implements:** `policy.tool_surface: code_python | code_javascript`

## Context

Code mode gives the model one `run_code` tool. Its script calls QQ's tools as
async functions, so one turn can batch reads, filter results and print a
summary. The script runs in an embedded interpreter: Monty (pure-Rust Python)
or Boa (pure-Rust JavaScript), selected per plan and behind one
`qq_script::Script` interface. Two questions are contested. Should the
interpreter run in the `qq` process or in a worker process? And what does a
crash mid-script leave behind? Both interpreters are safe Rust with no
filesystem, network or process access of their own. Monty warns that a
native stack overflow aborts the host. The measured cost of a fresh
interpreter per script is about 2 µs for Monty and 200 µs for Boa (p50,
release). A worker process costs a spawn per script or a pooled lifecycle to
supervise.

## Decision

Run scripts in-process. Each interpreter step runs on a blocking thread
(`spawn_blocking`), never on a Tokio worker; an interrupt abandons the step.
Monty runs on that thread directly; Boa runs on a dedicated thread behind
bounded channels, because a Boa `Context` is not `Send`. Every call a
script makes is a real `tool_calls` row with `parent_call_id` and
`script_seq`, inserted and published before the gate sees it. It then goes
through the same `ToolGate` and dispatcher as a model call. Interpreter
state is never persisted. A crash mid-script settles the `run_code` call and
its started children through the existing interrupted-run recovery; nothing
re-runs, and the model sees the interruption on its next turn.

Exposure is bounded by limits rather than isolation: recursion depth 200 in
both engines, a 16 MiB stack for the Boa thread, 10 s of interpreter time
(Monty's feed budget; a wall-clock wait per step for Boa), 256 calls,
64 KiB of printed output, and 256 KiB of MCP structured content per call.
Child calls count against the run's tool-call and cost budgets and pass
through the loop guard, so a script stops when the run would.

## Consequences

- Positive: no process management, no IPC encoding of tool results, and
  start-up cost that is negligible next to a model turn. Child calls get
  approval, audit, budget and replay behavior for free, because they are
  ordinary tool calls.
- Negative / risks: an interpreter bug that aborts, such as a native stack
  overflow below the recursion cap, takes down the server and every session
  it hosts, not just the run. Neither engine caps heap memory: Monty's
  `max_memory` needs its allocator installed, and Boa has no heap limit, so
  a script can allocate until the process runs out. A Boa script that
  exceeds its time limit is abandoned rather than stopped. A loop stops at
  50 M iterations, but loop-free code (exponential recursion) runs to the
  end on its thread. At most 8 Boa engines may be live per process, so the
  leak is bounded but real. An abandoned Monty step likewise keeps its
  blocking thread until Monty's own 10 s budget fires.
- Follow-ups: install `monty-alloc` (or a counting allocator) for a memory
  ceiling; evaluate Boa through `Script::evaluate_async_with_budget` so a
  stop flag can end an abandoned script. If the benchmark arms (`benchmarks/arms/m*`) show code mode is
  worth keeping, run a fuzzing pass against both engines before making it a
  default. Move scripts to a worker process if any abort reproduces.

## Alternatives considered

| Alternative | Why not (now) |
| --- | --- |
| Worker process per script | Adds a spawn (ms) per `run_code`, plus stdio framing for every child call and result; the isolation it buys covers an abort nobody has reproduced |
| Pooled worker process | Needs a supervisor, health checks and per-session reset to keep variables from leaking; that is too much machinery for an experiment |
| WASM (e.g. CPython or QuickJS in wasmtime) | Real isolation and fuel metering, but a large runtime dependency and slower start-up; reconsider if code mode ships by default |
| Wall-clock interrupt for Boa | Boa 0.22 exposes no instruction hook for modules; only `Script` evaluation can yield, and scripts lose `import` and top-level await |
| Persist interpreter state to resume a script after a crash | Re-running a half-finished script would repeat side effects the gate already approved; settling as interrupted is the safe answer |

## Evidence / references

- `crates/qq-script/src/{python.rs,javascript.rs}`: the two engines;
  `crates/qq-script/tests/conformance.rs` runs every shared case in each
  compiled-in language.
- `crates/qq-core/src/runtime/run_loop.rs` (`settle_tools`, the `run_code`
  branch) and `crates/qq-core/src/sessions/tool_calls.rs`
  (`request_script_call`): child calls persisted before execution.
- Store schema 66 (`tool_calls.parent_call_id`, `script_seq`); protocol 44
  (`ToolCallSnapshot.script`).
- Start-up and limit probe (release build, this machine): fresh Monty
  `print(1)` p50 1.9 µs, p95 3.1 µs; fresh Boa p50 197 µs, p95 363 µs.
  Unbounded recursion ends as a `Limit` failure in both, in 150 µs and
  270 µs. A 1 M-iteration loop takes 45 ms (Monty) and 40 ms (Boa).
- Release binary: 49,174,976 bytes at the merge base, 63,811,128 bytes with
  both engines (+14.6 MB, +30%), over ADR-0010's 48 MB default ceiling,
  which the base already exceeds.
