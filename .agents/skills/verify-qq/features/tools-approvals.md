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
  `read_tool_result`, `command_status`, `load_skill`, and MCP tools named
  `mcp__<server>__<tool>`.
- **Policy** (`crates/qq-core/src/approval.rs`, `sessions/approvals.rs`):

  | Mode | Behavior |
  | --- | --- |
  | `read-only` | denies Mutating, Shell, External, and Network, and does not send those schemas |
  | `ask` | holds every non-read call for a human; headless denies it immediately |
  | `auto` (default) | never holds; runs every call in the checkout except the fixed stops and managed denies, which are denied as tool errors |
  | `full` | never holds a root call; runs everything in the checkout, fixed stops included |

## How to get to it (user POV)

`/approval` in the TUI, or `--approval` on `qq run`.

## Driving it headlessly

| Script reply | Flags | Expect |
| --- | --- | --- |
| `{"tool":"write_file","args":{"path":"x.txt","content":"hi"}}` | `--approval read-only` | `tool_call_finished` with state `denied`; exit 0; no `x.txt` |
| same | (default `auto`) | state `completed`; `workspace/x.txt` exists after the run |
| `{"tool":"shell","args":{"command":"rm -f gone.txt"}}` | `--approval auto` | no approval events; state `completed`; `gone.txt` removed from `workspace/` |
| same | `--approval ask` | `tool_approval_requested`, then `tool_approval_resolved` with `denied` |
| `{"tool":"shell","args":{"command":"git push --force"}}` | `--approval auto` | state `denied`; the result starts `permission_denied: fixed_stop: ` |
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
  is not in Auto (check the mode) or the call is from a child of an Ask parent.
- Headless never waits on a held call. It denies it immediately.
- `ask_user` needs 2–6 options.
