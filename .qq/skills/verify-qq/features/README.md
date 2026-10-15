# Feature map

One file per user-facing feature. Each answers, from the user's side: what
it is, how to reach it, how to drive it with `qq-home.sh`, and what goes
wrong. `cargo xtask smoke` scenarios are named where they already cover a
path.

| Feature | File | Smoke scenario |
| --- | --- | --- |
| One-shot answer | [ask.md](ask.md) | `ask-streams-text` |
| Headless autonomous run | [run.md](run.md) | `run-tool-call`, `run-provider-401` |
| Server and SSE | [serve.md](serve.md) | `serve-sse` |
| Interactive terminal UI | [tui.md](tui.md) | (none; golden frames) |
| Sessions and resume | [sessions.md](sessions.md) | `run-resume-session` |
| Tools and approvals | [tools-approvals.md](tools-approvals.md) | `run-tool-call` (read-only path) |
| Delegation (child agents) | [delegation.md](delegation.md) | (none) |
| Compaction | [compaction.md](compaction.md) | (none) |
| MCP servers | [mcp.md](mcp.md) | (none) |
| Init, auth, config, doctor | [config-doctor.md](config-doctor.md) | `doctor-and-config` |

## Fake provider replies (`$QQ_HOME/script.jsonl`)

| Line | Effect |
| --- | --- |
| `{"text": "..."}` | streams text, then `response.completed` with usage |
| `{"tool": "NAME", "args": {...}}` | one `function_call` item; qq runs the tool and sends `function_call_output` next request |
| `{"status": 401, "body": "..."}` | HTTP error with a JSON body |

The SSE shapes it emits are the ones `crates/qq-provider/src/providers/openai.rs`
parses (`response.output_text.delta`, `response.output_item.added|done`,
`response.completed`); a stream without `response.completed` is a provider
error.
