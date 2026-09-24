# Headless: `qq ask`, `qq run`, `qq serve`

Three ways to use QQ without the TUI. All three share the runtime, tools,
approval policy, and session store with it.

## `qq ask` — one answer

```sh
qq ask "Explain what src/parser.rs does in three sentences"
qq ask --model openai/gpt-5.4-mini "Reply with pong"
```

Streams one model response to stdout. No tools, no session, nothing
persisted. Use it to check a credential or for quick questions from a
script. Exit `0` on success, `1` otherwise.

## `qq run` — the agent, unattended

```sh
qq run --approval auto "Add a --dry-run flag to the CLI and cover it with a test"
```

Runs one task to completion in the current directory (or `--workspace
PATH`) through a durable session. Progress goes to stderr, the final answer
to stdout.

### Approval without a human

| `--approval` | Meaning |
| --- | --- |
| `read-only` (default) | reads only; every edit, shell, MCP, and network call is denied and reported to the model |
| `auto` | edits inside the workspace, safe shell (`allow` verdict or a grant), MCP and granted hosts run; `prompt`-verdict shell is denied; `forbidden` refused |
| `full` | everything except `forbidden` shell shapes |

Under `read-only`, when at least one held call was denied, a text-mode run
ends with `held calls were denied under --approval read-only; rerun with
--approval auto to allow workspace edits` on stderr (after the answer, before
the resume hint). The exit status is unchanged; JSONL output carries no hint
because each denied call is already in its `tool_call_finished` record.

Between `auto` and `full`, grant exactly what the task needs:

```sh
qq run --approval auto \
  --allow-shell "cargo test" --allow-shell "cargo fmt --all" \
  --allow-tool write_file \
  --allow-host crates.io \
  "…"
```

Grants use the same shapes as the TUI's approve-for-session
([Permissions](permissions.md#grants-in-configuration)); `--allow-shell
"cargo test"` covers `cargo test -p x` and never `cargo test | sh`.

### Limits

| Flag | Effect | Exit |
| --- | --- | --- |
| `--timeout-seconds N` | cancel after N seconds of wall clock | 3 |
| `--max-turns N` | cancel when model turn N+1 would start | 3 |
| `--max-cost-usd V` | cancel when the estimated cost passes V; refused up front if the model has no pricing | 3 |

### Exit codes

| Code | Status | Meaning |
| ---: | --- | --- |
| 0 | `completed` | final answer produced (and satisfied `--output-schema` if given) |
| 1 | `task_failed` | the agent reported failure, the run failed, or the answer never satisfied the schema |
| 2 | `invalid_configuration` | QQ refused to start: config, model, credential, pricing, or flag error |
| 3 | `timed_out` / `budget_exhausted` | a limit was reached |
| 4 | `harness_failure` | QQ itself failed (store, provider protocol, internal) |
| 5 | `needs_input` | the agent asked a question and nobody was there; the question is in the event stream |
| 130 | `interrupted` | Ctrl-C |

### Machine-readable output

```sh
qq run --format jsonl --approval auto "…" > run.jsonl
qq run --trace run.jsonl "…"        # text on the terminal, JSONL to the file too
```

Each line is one JSON object with a `type`:

| `type` | When | Contents |
| --- | --- | --- |
| `trial` | once, first | QQ version and revision, `protocol_version`, workspace/session/run ids, model, profile, approval, limits, `correlation` labels |
| `event` | every protocol event | the same envelope the TUI consumes: prompts, model text, tool calls and results, approvals, usage |
| `outcome` | once, last | `status`, `exit_code`, token `usage`, `estimated_cost_usd_nanos`, `final_output` when a schema was given |

The shapes are versioned with `PROTOCOL_VERSION` and pinned by golden
fixtures under `crates/qq-protocol/tests/fixtures/headless/`; a consumer
should read `trial.protocol_version` first. Full contract:
[`../design/headless-contract.md`](../design/headless-contract.md).

### Structured answers

```sh
qq run --output-schema answer.schema.json --output-repair-turns 2 "…"
```

The final answer must be one JSON document valid against the schema (a
bounded subset: ≤ 64 KiB, no `$ref`). QQ gives the model up to N extra turns
to repair an invalid answer; the outcome's `final_output` carries either the
parsed value or the validation errors. Valid JSON is not a correct answer;
verify it.

### Continuing a session

```sh
qq run --session 01J… "Now add the same flag to the docs"
```

Submits into an existing idle root session of this workspace, keeping its
history. The model, profile, and approval come from *this* invocation, not
the session's past. Every exit prints the id you need.

### Steering from a pipe

`--steer-stdin` reads one line per steering message and injects each at the
run's next model/tool boundary; without it stdin is untouched.

### Labels for attribution

`--correlation job=nightly --correlation pr=123` (≤ 8, opaque to QQ) appear
on the `trial` record and every session snapshot.

### Profiles

`--profile NAME` selects a profile from configuration or a trusted pack; it
sets the model, approval mode, and tool catalog for the run and fails
before starting when the name is unknown.

## `qq serve` — a persistent server

```sh
qq serve                                # 127.0.0.1, random port
qq serve --bind 127.0.0.1:4711
qq serve --allow-origin https://app.example.com
```

Runs the user-scoped server in the foreground. The TUI starts one in the
background automatically when none is running; `qq serve` is for keeping
sessions alive across TUI restarts, for several clients on one machine, and
for remote clients over a private network. It prints `qq server listening
at ADDR`; a second `qq serve` reports the existing one.

The wire protocol is HTTP + SSE with resumable event cursors; see
[`../design/protocol.md`](../design/protocol.md). Remote authentication
beyond loopback is being designed
([`../plans/multi-surface-clients.md`](../plans/multi-surface-clients.md)).

## In CI

```yaml
- run: printenv ANTHROPIC_API_KEY | qq auth login anthropic --allow-file
  env: { ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }} }
- run: >
    qq run --model anthropic/claude-sonnet-5 --approval auto
           --allow-shell "cargo test" --timeout-seconds 900 --max-cost-usd 2
           --format jsonl "Fix the failing tests" > run.jsonl
```

Or skip the store entirely: QQ reads `ANTHROPIC_API_KEY` from the
environment when nothing is stored. `QQ_CONFIG_CONTENT='(version: 1, model:
"…")'` supplies configuration without a file. If the repository ships
`.qq/config.ron` with sensitive sections, run `qq trust` first.
