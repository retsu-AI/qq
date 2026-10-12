# run (headless)

## Sub-features

- **Flags** (`src/cli.rs`):
  - Input and workspace: `--workspace`, `--session`, `--steer-stdin`, `--correlation K=V` (at most 8).
  - Approval: `--approval read-only|auto|full` (default `read-only`), `--allow-tool`, `--allow-shell`, `--allow-host`.
  - Limits: `--timeout-seconds`, `--max-turns`, `--max-cost-usd`.
  - Output: `--output-schema`, `--output-repair-turns` (0–8), `--format text|jsonl`, `--trace PATH`.
- **Exit codes** (`crates/qq-protocol/src/headless.rs`):

  | Code | Meaning |
  | --- | --- |
  | 0 | completed |
  | 1 | task_failed |
  | 2 | invalid configuration |
  | 3 | timed out or budget exhausted |
  | 4 | harness or persistence failure |
  | 5 | needs input |
  | 130 | interrupted |

## How to get to it (user POV)

`qq run --approval auto "task"`. In text mode, progress goes to stderr
(`[tool] NAME …`) and the answer goes to stdout.

## Driving it headlessly

```sh
printf '%s\n' '{"tool":"read_file","args":{"path":"inventory.rs"}}' '{"text":"done"}' > $QQ_HOME/script.jsonl
.qq/skills/verify-qq/qq-home.sh qq <name> run --format jsonl "where is Widget?" > $QQ_HOME/evidence/run.jsonl
jq -r 'if .type=="event" then .envelope.event.type else .type end' $QQ_HOME/evidence/run.jsonl
```

The JSONL contains three record types:

- `trial`: carries `session_id` and `run_id`.
- `event`: the event is in `.envelope.event.type`.
- `outcome`: carries `status`, `exit_code`, `message`, `usage`, and `final_output`.

Golden examples are in `crates/qq-protocol/tests/fixtures/headless/v32/`.

| Behavior | How to trigger it | Expect |
| --- | --- | --- |
| Turn budget | `--max-turns 1` with a tool reply | exit 3, "exhausted its 1 model turn budget" |
| Cost budget | `--max-cost-usd 1` | exit 2 unless the model has `pricing` |
| Provider auth failure | `{"status":401}` reply | exit 1, `task_failed`, exactly one request (smoke `run-provider-401`) |

## Gotchas

- `qq run` owns the session store. A second `qq run`, a TUI, or a `qq serve`
  on the same home makes it exit 4. Use one home per concurrent drive.
- The resume hint only prints when stderr is a terminal.
