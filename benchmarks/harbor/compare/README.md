# Cross-harness Harbor summaries

`summarize.sh` prints Harbor `result.json` files side by side without
installing Harbor or invoking an agent:

```sh
benchmarks/harbor/compare/summarize.sh JOB_DIR [JOB_DIR ...]
```

Each immediate child of a job directory may contain a `result.json`. A trial
passes only when its verifier reward is at least `1` and `exception_info` is
null. Missing or null rewards fail; malformed or non-finite rewards reject the
input. Empty job directories produce a zero-trial row, while nonexistent paths
are rejected. Exceptions remain in the attempt count,
exception count, and pass-rate denominator even if a reward is positive. Thus a
verifier-accepted no-edit trial can pass, while an unfixable trial, a failed
verifier, or success prose without an independent positive reward cannot.

`$/attempt` and `$/pass` are printed only when every counted trial has a finite,
non-negative numeric `agent_result.cost_usd`. An explicit zero is known pricing;
a missing or null value is unknown. `$known` is always only the known subtotal,
and `unk$` is the number of attempts with unknown cost. Any unknown cost makes
both derived cost columns `-`, so the subtotal cannot be mistaken for a complete
total. Malformed and negative costs, and overflowing cost subtotals, stop the
command instead of being coerced. Large finite costs retain their available
floating-point precision rather than overflowing during display rounding.

Run the deterministic, credential-free parser regressions with:

```sh
benchmarks/harbor/compare/test-summarize.sh
```

The fixtures created by that test are synthetic parser controls, not model runs
and not evidence that an evaluator is isolated. In particular, their no-op,
unfixable, and fake-success labels test summary classification only.

## Limits on comparisons

A side-by-side summary does **not** certify that jobs match on model, task,
budget, source, or any other controlled variable, and it does not isolate the
value of QQ or JEV. Claims require the exact frozen model, provider and version;
harness and configuration; task and evaluator hashes; starting source; total
budgets; attempt IDs; and independent review. QQ off/on and JEV off/on are
separate experimental axes and must not be conflated.

This script reports fields already present in Harbor results. It does not prove
supplier charge containment, reconstruct missing costs, validate a broader
result schema, or establish the availability or superiority of any agent.
