# Headless: `qq ask`, `qq run`, `qq serve`

Three ways to use QQ without the TUI. All three share the runtime, tools,
approval policy, and session store with it.

## `qq ask` — one answer

```sh
qq ask "Explain what src/parser.rs does in three sentences"
qq ask --model openai/gpt-5.4-mini "Reply with pong"
```

Streams an answer to stdout using workspace read tools and configuration-granted
external tools. It cannot ask approval questions: built-in edits, commands,
questions and ungranted external/network tools are omitted. No session is
persisted. Use it to check a credential or for quick questions from a script.
Exit `0` on success, `1` otherwise.

## `qq run` — the agent, unattended

```sh
qq run --approval auto "Add a --dry-run flag to the CLI and cover it with a test"
```

Runs one task to completion in the current directory (or `--workspace
PATH`) through a durable session. Progress goes to stderr, the final answer
to stdout.

### Foreground goals

```sh
qq run --goal "Finish the migration" --check 'cargo test' --format jsonl
qq run --goal --session 1f0c9a2e4b7d4c1e9a3f5b6d7e8f9012 --check 'cargo test' --goal-check-approval once --format jsonl
```

Auto authorizes the exact check within its native task scope without a separate
approval. Every execution rechecks current authority and containment; it cannot
fall back to host execution. Missing containment, forbidden commands, or policy
denials refuse admission instead of asking for approval.

Other modes retain separate check consent: `once` permits one execution and
`for-goal` permits reuse for this exact check. An ordinary shell allowlist does
not authorize a goal check. Read-only mode never executes shell checks.

`--goal-deadline 24h`, `--goal-max-runs 20` and `--max-cost-usd 5` bound the
goal; workspace defaults apply when omitted. Timeout and turn flags constrain
each run. With a new objective, unspecified limits/check fields survive an
edit; `--replace` explicitly starts a replacement. Bare `--goal --session ID`
continues the stored specification. It leaves a missing-consent pause intact.

The foreground client uses the same durable runtime driver as the server.
It stops on a non-active state, including an unanswered question, without
inventing an answer. Goal exits are achieved0, paused6, blocked7, exhausted8,
interrupted130; setup and harness failures retain2/4. JSONL includes workspace
events, `goal_run` records for settled direct runs, then a final `goal` snapshot
with the state/reason and exit code. A stopped goal drains its remaining work
before that final record, bounded by the shutdown grace period; unrelated runs
stay running. After the final goal record, the process retains ownership until
other accepted runs settle naturally before closing. A further Ctrl-C cancels
and drains that remaining work; it preserves the selected goal's final record
and exit status. A failure to close is reported on stderr and preserves that
selected exit status. Helpers remain event records. Refused verification attempts also
have run records; the audit counter and check result distinguish them from
executed checks. A paused goal persists for subsequent resume. Goal mode has no steering pipe or typed
answer schema; those flags apply to ordinary prompt runs.

### Approval without a human

| `--approval` | Meaning |
| --- | --- |
| `read-only` | reads only; every edit, shell, MCP, and network call is denied and reported to the model |
| `ask` | reads run; calls requiring human approval are denied immediately, even with matching allowlists |
| `auto` (default) | edits inside the workspace, safe shell (`allow` verdict or a grant), MCP and granted hosts run; `prompt`-verdict shell is denied; `forbidden` refused |
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
Under Auto, these grants are stored before the prompt is submitted, including
when resuming a session. They do not open an approval hold. Read-only, Ask,
parent authority, forbidden commands, and managed denies still apply.

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
| 4 | `harness_failure` | QQ itself failed (store, provider protocol, internal), or another `qq` process owns the session store |
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
qq run --session 1f0c9a2e4b7d4c1e9a3f5b6d7e8f9012 "Now add the same flag to the docs"
```

Submits into an existing idle root session of this workspace, keeping its
history. The model and profile come from this invocation. The stored approval
mode is preserved unless you explicitly pass `--approval`. New sessions
default to Auto. Every exit prints the id you need.

When reopening after a coordinator crash, it waits for that session's inherited
native commands and includes their saved results before starting the new
prompt. Commands keep their original deadlines and are not executed again.
Ctrl-C during this wait cancels and drains the inherited run, pauses its owning
goal, and exits with status 130.

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

Runs the user-scoped server in the foreground until `Ctrl-C`. Without it,
each `qq` starts a server inside itself when none is running, and quitting
that `qq` cancels its runs ([Exiting](tui.md#exiting)). `qq serve` is for
keeping runs going after the TUI quits, for several TUIs on one machine, and
for remote clients over a private network. It prints `qq server listening
at ADDR`; a second `qq serve` reports the existing one. Stopping it cancels
every queued and running run, as quitting an owning TUI does.

`qq run` and `qq ask` never use a server. `qq run` opens the session store
itself, so it cannot run while another process holds that store — a TUI,
`qq serve`, or another `qq run` — and exits `4` with `session store is owned
by another running qq process`. Queue the work from the TUI, run jobs one
after another, or give concurrent CI jobs separate data directories
(`XDG_DATA_HOME` on Linux).

The wire protocol is HTTP + SSE with resumable event cursors; see
[`../design/protocol.md`](../design/protocol.md). The server binds only to
loopback; there is no remote client or remote authentication yet.

## Managing server goals from scripts

Start `qq serve`, then use the session ID from the TUI or server API:

```sh
qq goal set "Finish the migration" --session 1f0c9a2e4b7d4c1e9a3f5b6d7e8f9012 --check 'cargo test' --goal-deadline 24h --goal-max-runs 20
qq goal show --session 1f0c9a2e4b7d4c1e9a3f5b6d7e8f9012 --json
qq goal approve-check once --command 'cargo test' --session 1f0c9a2e4b7d4c1e9a3f5b6d7e8f9012
qq goal pause --session 1f0c9a2e4b7d4c1e9a3f5b6d7e8f9012
```

Review the check before authorizing it. `once` grants one execution;
`for-goal` grants reuse for that goal's current check. Editing a goal preserves
unspecified budget and check fields. `--replace` starts a replacement with
workspace defaults. `resume` and `clear` are also available. These commands
use authenticated local server discovery and leave store ownership with the
server. They return the goal snapshot (`show`) or command receipt (mutations),
without waiting for the goal to finish. See [CLI flags](cli.md#qq-goal).

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

If a foreground goal encounters a harness failure after accepting a goal, its
final JSONL goal record uses `harness_failure` and exit code `4` when output is
still writable and no final write has been attempted. A final write is never
retried, because an output error can leave a partial record. Once the final
stdout record is flushed, later trace or runtime-close failures are reported
on stderr and preserve that record's exit code. The record includes the current snapshot of that same goal when
available, otherwise its last observed snapshot. This fallback supplies context;
it does not assert that the goal is still current or reached a terminal state.
Clearing or replacing the goal never substitutes the replacement in this record.
