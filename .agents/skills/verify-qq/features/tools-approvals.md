# tools and approvals

## Sub-features

- **Built-in tools** (`crates/qq-core/src/tools/specs.rs`):

  | Effect | Tools |
  | --- | --- |
  | ReadOnly | `read_file`, `tree`, `search` |
  | Mutating | `edit_file`, `write_file` |
  | Shell | `shell`, `exec` |
  | Interactive | `ask_user` |
  | Network | `fetch` |

  Also: `spawn_agent`, `wait_agents`, `cancel_agent`, `search_history`,
  `read_tool_result`, MCP tools named `mcp__<server>__<tool>`, and, in an
  Auto root run inside a Git checkout, `task_commit`, `task_push`, `task_pr`.
- **Policy** (`crates/qq-core/src/approval.rs`, `sessions/approvals.rs`):

  | Mode | Behavior |
  | --- | --- |
  | `read-only` | denies Mutating, Shell, External, and Network, and does not send those schemas |
  | `ask` | holds every non-read call for a human; headless denies it immediately |
  | `auto` (default) | never holds; runs edits and any shell in the private task copy under the native sandbox, publishes on completion; out-of-bounds calls are denied as tool errors |
  | `full` | never holds a root call; runs in the checkout without the task sandbox; shell still goes through the command worker |

- **Sandbox:** shell needs the native backend in `ask`, `auto`, and `full`
  (see `docs/guide/install.md#system-requirements`). On a host that blocks
  user namespaces the first shell call fails the runtime (exit 4) and holds
  the workspace.

## How to get to it (user POV)

`/approval` in the TUI, or `--approval` on `qq run`. Session grants come from
`--allow-tool`, `--allow-shell`, and `--allow-host`.

## Driving it headlessly

| Script reply | Flags | Expect |
| --- | --- | --- |
| `{"tool":"write_file","args":{"path":"x.txt","content":"hi"}}` | `--approval read-only` | `tool_call_finished` with state `denied`; exit 0; no `x.txt` |
| same | (default `auto`) | state `completed`; `workspace/x.txt` exists after the run |
| `{"tool":"shell","args":{"command":"rm -f gone.txt"}}` | `--approval auto` | no approval events; state `completed`; `gone.txt` removed from `workspace/` after the run publishes |
| same | `--approval ask` | `tool_approval_requested`, then `tool_approval_resolved` with `denied` |
| `{"tool":"ask_user","args":{"questions":[{"prompt":"Which?","options":["a","b"]}]}}` | any | outcome `needs_input`, exit 5 |

Rewriting `script.jsonl` restarts the fake's reply sequence, so you can run
the rows one after another in a single home. Read the state with:

```sh
jq -c 'select(.type=="event").envelope.event | select(.type|startswith("tool_")) | [.type, .tool_call.state]' run.jsonl
```

The `ask_user` row emits `tool_approval_requested`, then
`tool_call_finished` with state `interrupted`.

The side effect is the proof. Check `workspace/` and
`select name, state from tool_calls` in the store.

## Gotchas

- Auto never emits `tool_approval_requested`. If you see one, the session
  is not in Auto (check the mode) or the call is a child's supervised call.
- Headless never waits on a held call. It denies it immediately, or after
  a 20 s grace period when a reviewer is configured.
- Auto edits land in a private task copy and are published only when the
  run completes; check `workspace/` after the run exits, not mid-run.
- `ask_user` needs 2–6 options.
