# delegation (child agents)

## Sub-features

- `spawn_agent {task, model?, role?, authority?}`. The schema is in
  `crates/qq-core/src/tools/specs.rs`.
  - `task` is required.
  - `role` exists only when a roster is configured.
  - `authority: "write"` exists only when `write_children` is set.
  - Unknown keys are rejected.
- `wait_agents {ids?, timeout_seconds}` and `cancel_agent {id}`.
- The spawn does not block: the parent continues while the child works.

## How to get to it (user POV)

The model calls the tool. In the TUI, use `/agents` or `Alt-C`. Configure it
with `delegation: (roster, default_role, max_depth, write_children)`.

## Driving it headlessly

```sh
printf '%s\n' '{"tool":"spawn_agent","args":{"task":"Say hi"}}' '{"text":"hi"}' > $QQ_HOME/script.jsonl
.qq/skills/verify-qq/qq-home.sh qq <name> run --format jsonl "delegate a greeting" > $QQ_HOME/evidence/d.jsonl
jq -c 'select(.type=="event") | [.envelope.session_id[0:6], .envelope.event.type]' $QQ_HOME/evidence/d.jsonl
```

Expected sequence:

1. The parent emits `tool_call_requested` for `spawn_agent`.
2. The child emits `session_created`, with `session.parent_id` set to the
   parent, then `prompt_queued`, `run_started`, and `run_finished`.
3. The parent emits `tool_call_finished`, takes another turn, and ends with
   `run_finished`.

The child's request in `provider-requests.txt` carries `Say hi` and has no
spawn tools.

## Gotchas

- It is on by default (`max_depth: 1`), even under `read-only`.
- The fake answers in request order. Parent and child requests interleave,
  so tell them apart by their input text.
- Under `--tui-qa-root`, delegation is refused.
