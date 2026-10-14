# Agent feedback loop

## Status

AF0–AF4, AF7, AF8 and the first part of AF9 are in review as one stack
(ENG-1028, branch `build/eng-1028-agent-feedback-loop`). AF5, AF6 and AF10
are planned; AF6.1 audits the cancellation module in a PR stacked on #282.
The parent AF6 audit stays open for the remaining modules.

## Goal

An agent gets fast, readable feedback on whether its change works, and the
repository's rules are checked by tools rather than by memory. Concretely:

- After an edit, `cargo xtask check` runs fmt, lint, clippy and tests on only
  the packages the change can affect. It reports each failure with its panic
  and the command that reruns it.
- `cargo xtask smoke` drives the real `qq` binary end to end with no network
  or credentials.
- The full suite runs in the merge queue, not on every push.
- `AGENTS.md` routes an agent to the `qq-mode` skill. That skill gives an
  exact playbook per task type and names the gate that proves each step.

## Baseline (AF0, 2026-10-11, 32-core host, loaded)

| Measure | Before | After AF1–AF2 |
| --- | ---: | ---: |
| `cargo test --workspace` (warm) | 140 s | n/a |
| `cargo nextest run --workspace` (warm) | 97 s, 1 timeout failure | 44–81 s, all pass |
| One-line `docs/` edit, rebuild tests | 4.5 s (relinks `qq`) | 0.14 s |
| One-line `qq-core` edit, rebuild tests | 6.8 s | 5.4 s |
| One-line root `src/` edit, rebuild tests | 6.4 s | 1.9 s |
| `target/debug/qq` size | 595 MB | 189 MB |
| `qq-tui` change, full gate incl. clippy | 140 s+ (tests alone) | 46 s (`cargo xtask check`) |

Under the old ordering, the slowest `qq-core` session suites (7–13 s each
when run alone, up to 69 s under load) ran last. One of them hit its 30 s
internal timeout.

## Slices

| Slice | Goal | Acceptance |
| --- | --- | --- |
| AF0 | Baseline before changes | Numbers above; raw logs under `target/qq-perf/af0/` |
| AF1 | Faster compile and link | Debug profile with line tables only and no debug info for deps; `build.rs` watches tracked files only for release builds or `QQ_EXACT_REVISION=1`; docs edits no longer relink. Linker unchanged: Rust ≥ 1.90 already uses rust-lld on x86_64 Linux |
| AF2 | nextest | `.config/nextest.toml` with `default`, `quick` and `ci` profiles; long suites get priority; whole suite green under nextest |
| AF3 | `cargo xtask check` | Affected selection from git diff plus reverse dependencies; root inputs select everything; `last.json` with failures and rerun commands; `--evidence`; unit tests for selection and failure parsing |
| AF4 | `cargo xtask smoke` | Six scenarios against a loopback fake; failing scenarios keep evidence; an injected bug (empty tool result) fails `run-tool-call` |
| AF5 | Structured tracing (`QQ_LOG`) | Spans at run, turn, provider, tool and store boundaries; JSON log under state dir; off by default; redaction test; `perf check` within budget. Needs an ADR (reserve in `root.md`) |
| AF6 | Test audit | Per module: stress for flakes, replace fixed sleeps (lower `SLEEP_CEILING`), `cargo mutants` to find tests that catch nothing, merge duplicates into tables; one module per PR with mutation catch count kept |
| AF7 | CI | PR runs the affected gate plus smoke; `merge_group` and `main` run the full gate and platform jobs; `ci:full` label; concurrency cancellation; artifacts on failure. Merge queue must be enabled in branch protection (operator) |
| AF8 | Agent instructions | `AGENTS.md` as router plus a Rules → enforcers table; `qq-mode`, `principles` and `verify-qq` skills; `qq-home.sh`; feature map for ten features; `docs/runbooks/testing.md` |
| AF9 | Encode lessons | `cargo xtask lint` with six rules (shipped). Next: mine recent PR review corrections for repeated mistake classes and add rules or types for each |
| AF10 | Prove the skills | Replay three to five past tasks with fresh agents using `qq-mode` and with the old `AGENTS.md`; compare time to green, gate iterations, review findings and whether evidence was produced |

## Order

AF5, AF6 and AF9-next are independent once this stack merges. AF10 runs
after at least one AF6 module lands, so the eval exercises a cleaner suite.
