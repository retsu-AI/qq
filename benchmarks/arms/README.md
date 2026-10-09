# Delegation Experiment Arms

Configuration overlays for the paired evaluation in
`docs/plans/supervised-delegation.md` §D6. Each file is a complete RON
document applied through `QQ_CONFIG_CONTENT` on top of the operator's normal
layers, so every arm shares the same providers, credentials, primary model,
and policy and differs only in the section it names. Routes below are
placeholders: replace `PROVIDER/...` with authenticated routes (see
`qq config show`, or `benchmarks/harbor/eval-config.ron` for the gateway
routes) before running, keeping the *same* primary `--model` for every arm
(`eval compare` refuses arms whose model differs).

| Arm | File | Question |
| --- | --- | --- |
| A0 | `a0-no-delegation.ron` | baseline: the root never delegates |
| A1 | `a1-read-children-same-model.ron` | does orchestration alone help |
| A2 | `a2-read-children-fast-worker.ron` | does cheap breadth lower $/pass |
| A3 | `a3-depth-two.ron` | does recursion add anything over A2 |
| B1 | `b1-audit-heuristic.ron` | does the audit raise pass rate; at what cost |
| C1 | `c1-write-children.ron` | do supervised writers finish more tasks |
| T1 | (no overlay) | continuation: compare a build before and after D1 |

Every arm turns the audit off except B1, so audit spend never confounds a
delegation comparison. A1, A2, A3, B1, and C1 are each compared against the
A0 baseline, and A3 also against A2 (does depth add anything over one level).
T1 has no configuration knob (continuation has no off switch); it
is a build-to-build comparison on the long-output task subset, and `compare`
tolerates the differing `qq_source_revision`.

## Running one arm

```sh
export QQ_CONFIG_CONTENT="$(cat benchmarks/arms/a2-read-children-fast-worker.ron)"
cargo xtask eval run \
  --arm A2 --job-name deleg-a2-seed1 \
  --model PROVIDER/PRIMARY \
  --dataset terminal-bench/terminal-bench-2 \
  --include-task-name ... \
  --n-attempts 3 --n-concurrent 2 \
  --timeout-seconds 900 --max-turns 200 --max-cost-usd 5 \
  --machine-class linux-x86_64-ci
```

`--arm` stamps the trial records; `QQ_CONFIG_CONTENT` is forwarded into the
task container by the Harbor adapter. Use a fresh `--job-name` per arm and
seed; jobs are never resumed. Classify every non-passing trial with
`cargo xtask eval classify` before `eval report` or `eval compare` will accept
the job.

## Comparing

```sh
cargo xtask eval compare \
  --baseline target/qq-eval/jobs/deleg-a0-seed1 \
  --candidate target/qq-eval/jobs/deleg-a2-seed1 \
  --output target/qq-eval/a0-vs-a2.json
```

Decisions follow the gates in `supervised-delegation.md` §D6: delegation
defaults per task class need a `cost_per_pass_ratio_ci95_high` below 0.80
with no meaningful pass-rate loss (`mcnemar_p_value` and `delta.pass_rate`);
`audit.mode` stays `heuristic` only if B1 raises pass rate or lowers $/pass,
otherwise the config default flips to `off`; depth above one stays opt-in
unless A3 beats A2.

## Before spending

Run the deterministic pre-checks first. They exercise every arm's events,
accounting, and ATIF conversion with scripted providers and cost nothing:

```sh
cargo test -p qq-core --lib -- \
  max_depth_zero depth_two write_children a_mutating_run_is_audited \
  children_receive_the_parents_remaining_budget
cargo test -p xtask -- eval::tests
cargo test -p qq --test harbor_atif_fixtures
```
