# QQ Harbor Evaluation Adapter

This directory contains QQ's repository-owned Harbor installed-agent adapter
and the deterministic QQ JSONL-to-ATIF conversion. It is evaluation tooling,
not a shipping Python package or a second agent runtime: Harbor invokes the
ordinary durable `qq run` path inside each task container.

Harbor is pinned to `0.20.0` in `requirements.txt`. The pin matters because
the installed-agent lifecycle, result schema, and ATIF validator are part of
the evaluation contract. The Nix dev shell provides `uv`, Python, and a musl
cross toolchain; install the exact Harbor release in an isolated tool
environment and put `uv`'s tool directory on `PATH`:

```sh
nix develop
uv tool install 'harbor==0.20.0'
export PATH="$HOME/.local/bin:$PATH"
harbor --version
```

## The Binary Uploaded To Task Containers

The adapter uploads a prebuilt `qq` into each task container. Task images span
many base distributions, so build the static musl target rather than the host
glibc binary; `cargo xtask eval run --target x86_64-unknown-linux-musl` does
this and records the target in the launch manifest. The dev shell exports the
cross linker (`CC_x86_64_unknown_linux_musl`,
`CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER`) and `rust-toolchain.toml`
installs the target's standard library, so the build is:

```sh
cargo build --release --target x86_64-unknown-linux-musl --bin qq
```

Most task images also ship without `ca-certificates`. The adapter uploads the
Harbor host's PEM trust store (or `QQ_CA_BUNDLE_PATH`, or certifi's bundle as
a last resort) to `/installed-agent/ca-certificates.crt` and points
`SSL_CERT_FILE` at it so provider TLS verification works everywhere.

## Approval Policy Inside Containers

`qq run` inside the container uses `--approval full` by default: task
containers are disposable, and the reference harnesses run unrestricted, so
`full` is the comparable setting. Pass `--approval auto` (or `read-only`) to
`cargo xtask eval run` for ablations; `auto` denies dangerous shell shapes such
as `rm -r`, `kill`, `chown`, `sudo`, and `git push`, which many Terminal-Bench
tasks legitimately need. The policy is recorded in the manifest and stamped on
every trial, so `eval report` refuses to mix them.

## Routing Through A LiteLLM Gateway

`benchmarks/harbor/eval-config.ron` is a secret-free, self-contained QQ
configuration for runs through the operator's LiteLLM gateway. It declares the
`litellm` provider with `auth: ApiKey(Env("LITELLM_API_KEY"))` and the gateway
model ids with their context windows, output limits, and per-token pricing
(including cache rates, which `--max-cost-usd` requires). Deliver it to the
container with `QQ_CONFIG_CONTENT`; the container has no other QQ
configuration, so the route is exactly what the file says:

```sh
export LITELLM_API_KEY=...            # never committed; forwarded, never logged
export QQ_CONFIG_CONTENT="$(cat benchmarks/harbor/eval-config.ron)"
cargo xtask eval run --model litellm/us.anthropic.claude-sonnet-5 ...
```

Built-in providers (`anthropic/...`, `openai/...`, `google/...`, `xai/...`)
need only their own API-key variable and no `QQ_CONFIG_CONTENT`.

## Validate The Adapter

The converter fixtures cover text-only completion, a tool loop, failure,
cancellation, compaction, and a parent/child run; the adapter tests cover the
container command, approval validation, and environment passthrough. Run them
in an environment that contains the pinned Harbor release:

```sh
PYTHONPATH=benchmarks/harbor \
  uv run --with 'harbor==0.20.0' --no-project \
  python -m unittest benchmarks.harbor.tests.test_atif benchmarks.harbor.tests.test_agent
cargo test --test harbor_atif_fixtures
cargo test -p xtask -- eval::
```

The Python suite converts every trace twice, exercises malformed input and
sub-agent references, and submits every generated document to Harbor's
published ATIF validator. Conversion or validation failure invalidates a
trial; the adapter does not silently return a passing result without a durable
trajectory.

## Run A Baseline

Use the repository task instead of constructing Harbor arguments by hand:

```sh
cargo xtask eval run \
  --model litellm/us.anthropic.claude-sonnet-5 \
  --dataset terminal-bench/terminal-bench-2 \
  --job-name qq-fixed-model-baseline \
  --target x86_64-unknown-linux-musl \
  --n-attempts 1 \
  --n-concurrent 2 \
  --timeout-seconds 900 \
  --max-turns 200 \
  --max-cost-usd 5 \
  --machine-class linux-x86_64-workstation
```

Use `--path` instead of `--dataset` for a local task or dataset and repeat
`--include-task-name` for a bounded smoke selection. `--dry-run` prints the
complete non-secret launch plan without building QQ or starting Harbor. A real
run:

1. verifies the pinned Harbor version;
2. records the current QQ revision into a release build of the selected target;
3. launches Harbor with `qq_harbor.agent:QQAgent`;
4. writes generated jobs under `target/qq-eval/jobs` by default; and
5. creates a fresh job directory and stores the exact launch manifest before
   Harbor starts, so setup failures retain their launch identity; and
6. relies on Harbor's resolved job/trial locks for task, resource, timeout,
   environment, and agent identity.

An existing job directory is rejected rather than resumed implicitly. Use a
fresh `--job-name` for each comparison run.

Two deadlines apply to each trial: QQ's own `--timeout-seconds` and Harbor's
per-task agent timeout (`timeout_sec` in `task.toml`, 900 s for most
Terminal-Bench 2 tasks). QQ must settle first so the trace ends with an
outcome record; `eval run` therefore passes `--agent-timeout-multiplier`
(default 1.5) to Harbor, and `--timeout-seconds` should stay at or below the
task's own limit. When the outer deadline fires first Harbor stops the
container mid-run, the adapter records `qq_status: interrupted_externally`,
and `eval report` counts the trial as a harness failure.

The repository-local adapter smoke task is intentionally tiny and has no
task-specific benchmark knowledge:

```sh
cargo xtask eval run \
  --model litellm/us.anthropic.claude-sonnet-5 \
  --path benchmarks/harbor/smoke-task \
  --job-name qq-adapter-smoke \
  --target x86_64-unknown-linux-musl \
  --n-attempts 1 \
  --n-concurrent 1 \
  --timeout-seconds 120 \
  --max-cost-usd 1
```

This is still a real model run and therefore requires deliberate credentials
and spend. Repository tests validate its Harbor configuration but do not run it.
The first credentialed smoke (2026-09-04, Sonnet 5 through LiteLLM) passed in
30 s for $0.015.

Provider credentials are inherited by the process and passed to the task
container by the adapter. They are never included in the launch plan or
report. Do not put credentials or generated jobs in this directory. Note that
`harbor datasets download` writes into the current directory; run it outside
the repository or delete the result.

## Classify And Report Failures

Every non-passing attempt needs one primary category backed by an exact
identifier-bearing field (for example a step, event, run, tool-call, trial, or
task identifier) in `agent/trajectory.json`, `agent/qq-trace.jsonl`, or
`result.json`. For example:

```sh
cargo xtask eval classify target/qq-eval/jobs/qq-fixed-model-baseline/TRIAL \
  --category verification-omitted \
  --evidence trajectory:4 \
  --note 'The trajectory ends after mutation without a verification step.'

cargo xtask eval report target/qq-eval/jobs/qq-fixed-model-baseline \
  --output target/qq-eval/qq-fixed-model-baseline.report.json
```

The report refuses missing or ungrounded classifications, hashes Harbor's job
and per-trial locks/configuration, and rejects trials whose QQ revision, model
route/organization, approval policy, prompt hashes, tool hash, or generation
limits differ. It reports reward/pass rate with a 95% Wilson interval, dollars
per attempt/pass, total and uncached tokens per pass, median/p95 wall time for
passing tasks, harness-failure rate, per-trial identities, and failure-category
counts. A Harbor setup failure with no QQ trace remains an explicit harness
failure rather than making the whole report unreadable. No category is inferred
from final answer text.

Each per-trial row also carries its own cost, total and uncached tokens, wall
time, reasoning tokens when the provider broke them out, the number of
sub-agents the run spawned, how many turns the provider cut at its output limit
and the runtime continued, and whether the run settled as
`provider_output_truncated`. These come from the durable QQ trace, which is the
only place they exist.

## Compare Two Arms

A paired comparison answers "did this configuration change help?" with the
same model on the same tasks. Run each arm as its own job with a distinct label
and express the configuration through `QQ_*` environment passthrough, which the
adapter forwards into the task container:

```sh
cargo xtask eval run --arm A0 --job-name deleg-a0 ... 
QQ_CONFIG_CONTENT="$(cat benchmarks/arms/delegation.ron)" \
  cargo xtask eval run --arm A2 --job-name deleg-a2 ...

cargo xtask eval compare \
  --baseline target/qq-eval/jobs/deleg-a0 \
  --candidate target/qq-eval/jobs/deleg-a2 \
  --output target/qq-eval/deleg-a0-vs-a2.json
```

`--arm` sets `QQ_EVAL_ARM`, which `qq run` stamps on the trial record; it never
changes behavior. Ready-made overlays for the delegation experiment live in
`benchmarks/arms/` with their own runbook. `compare` refuses jobs that differ in model route,
organization, output-token limit, context window, approval policy, run limits,
prompt version, instruction hash, workspace identity, machine class, or Harbor
configuration, and jobs whose arms carry the same label. It tolerates — and
lists under `tolerated_differences` — the arm label, QQ version and revision,
system-prompt and tool-schema hashes, and selected guidance, since those are
exactly what an arm changes. Attempts pair task by task in trial-name order;
a task with different attempt counts or checksums across arms is an error.

The comparison reports the discordant-pair table (`both_passed`,
`both_failed`, `baseline_only`, `candidate_only`) with a two-sided exact
McNemar p-value, both arms' scorecards and their delta, and the candidate's
dollars-per-pass as a ratio of the baseline's with a percentile bootstrap
interval over task pairs (deterministic from `--seed`; `--resamples` defaults
to 2000). A ratio below 1.0 favors the candidate. The R7 promotion gate in
`docs/plans/terminal-bench-readiness.md` reads: ratio interval entirely below
0.80 with no meaningful pass-rate loss.

## Compare Against Other Harnesses

`compare` only pairs QQ jobs because it needs the QQ trace identity. To place
QQ against Claude Code, Codex CLI, or OpenCode, run those harnesses through
Harbor's built-in adapters on the same model, task list, concurrency, and
machine, then read all jobs with the harness-neutral summary:

```sh
export LITELLM_BASE_URL=https://GATEWAY/v1 LITELLM_API_KEY=...
TASKS=(-i cobol-modernization -i fix-git -i sqlite-db-truncate)

cargo xtask eval run --model litellm/us.anthropic.claude-sonnet-5 \
  --dataset terminal-bench/terminal-bench-2 --job-name pilot-qq \
  --target x86_64-unknown-linux-musl --n-concurrent 2 \
  --timeout-seconds 900 --max-turns 200 --max-cost-usd 5 "${TASKS[@]}"
benchmarks/harbor/compare/run-reference.sh claude-code us.anthropic.claude-sonnet-5 pilot-cc \
  --n-concurrent 2 "${TASKS[@]}"
benchmarks/harbor/compare/run-reference.sh codex us.anthropic.claude-sonnet-5 pilot-codex \
  --n-concurrent 2 "${TASKS[@]}"
benchmarks/harbor/compare/run-reference.sh opencode us.anthropic.claude-sonnet-5 pilot-oc \
  --n-concurrent 2 "${TASKS[@]}"

benchmarks/harbor/compare/summarize.sh target/qq-eval/jobs/pilot-*
```

`run-reference.sh` maps the gateway onto each adapter's expected environment
(`ANTHROPIC_BASE_URL` for Claude Code, `OPENAI_BASE_URL` for Codex and
OpenCode) and passes any further arguments to `harbor run`. Reference
harnesses report cost only when their own accounting works through the
gateway; a `-` in the `$/pass` column means the adapter produced no cost, not
that the run was free. Keep the public leaderboard
(<https://www.tbench.ai/leaderboard/terminal-bench/2.1>) as a secondary
reference only: it uses `k=5` on different hardware and often different model
snapshots, so paired local runs are the comparison that supports a decision.
