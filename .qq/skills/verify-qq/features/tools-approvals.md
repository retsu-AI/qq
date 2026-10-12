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
  `read_tool_result`, and MCP tools named `mcp__<server>__<tool>`.
- **Policy** (`crates/qq-core/src/approval.rs`):

  | Mode | Behavior |
  | --- | --- |
  | `read-only` | denies Mutating, Shell, External, and Network, and does not send those schemas |
  | `auto` | runs edits; holds shell unless the classifier allows it or a grant matches; holds ungranted hosts |
  | `full` | runs everything except forbidden commands |

## How to get to it (user POV)

`/approval` in the TUI, or `--approval` on `qq run`. Session grants come from
`--allow-tool`, `--allow-shell`, and `--allow-host`.

## Driving it headlessly

| Script reply | Flags | Expect |
| --- | --- | --- |
| `{"tool":"write_file","args":{"path":"x.txt","content":"hi"}}` | (read-only) | `tool_call_finished` with state `denied`; exit 0; no `x.txt` |
| same | `--approval auto` | state `completed`; `workspace/x.txt` exists |
| `{"tool":"shell","args":{"command":"rm -f gone.txt"}}` | `--approval auto` | `tool_approval_requested`, then `tool_approval_resolved` with `denied` |
| `{"tool":"ask_user","args":{"questions":[{"prompt":"Which?","options":["a","b"]}]}}` | any | outcome `needs_input`, exit 5 |

The side effect is the proof: check `workspace/` and
`select name, state from tool_calls` in the store.

## Gotchas

- Headless never waits on a held call. It denies it immediately, or after
  a 20 s grace period when a reviewer is configured.
- `ask_user` needs 2–6 options.
