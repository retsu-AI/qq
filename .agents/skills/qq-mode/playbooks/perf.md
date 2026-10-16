# Playbook: perf

1. **Baseline before any change**, per `docs/runbooks/perf-recording.md`.
   Pick the narrowest gate that measures the user-visible cost:
   `cargo xtask perf check` (startup, first token, server readiness, RSS,
   binary size) or a bench (`cargo bench -p qq-provider --bench
   provider_compiler`, `cargo bench -p qq-core --bench <name>`). Keep raw
   output under `target/qq-perf/<slice>-<date>/`.
2. **Explain the number** before acting on it: what limits it (CPU, I/O,
   lock, allocation), run-to-run variance (repeat, A/A on the same binary),
   and whether it measures the path users hit. Load the
   `principles` skill, section *Explain the number*.
3. **One hypothesis per change.** Make it, re-measure, keep it only if the
   gain exceeds the A/A noise. Revert losers.
4. **Keep correctness gates green** (`cargo xtask check`). A faster wrong
   answer is a regression.
5. **Post-change measurement** next to the baseline, same host, same
   command. Budgets live in `benchmarks/perf/budgets-v1.json`.
6. Run `.qq/skills/qq-mode/playbooks/opening-a-pr.md` with before/after numbers and the noise
   floor in the body.

**Reply:** the user-visible latency or resource change with before, after,
and noise, what limited it, and what you tried that did not help.
