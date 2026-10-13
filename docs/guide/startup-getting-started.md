# Get started with the stack

Start with the smallest local QQ journey, then choose the source-backed
extension that matches the work. The detailed QQ guides remain authoritative
for installation, providers, configuration, and CLI behavior.

## First local journey

Follow [Install](install.md) and [Quickstart](quickstart.md), then verify the
local binary and project boundary:

```sh
qq --version
qq doctor
qq config paths
qq ask "Reply with pong"
```

The first command identifies the binary. `doctor` reports local readiness;
`config paths` shows where QQ reads its configuration. The `ask` command
exercises the configured agent path. A local response does not establish a
hosted deployment or durable customer workflow.

For an interactive session, run `qq`. For a bounded script, use
`qq run --approval auto "Summarize the README"` only when the repository's
approval policy allows it. Review [Headless](headless.md) and
[Permissions and trust](permissions.md) before using automation. Never copy
credentials, session databases, or raw prompts into documentation.

## Choose a surface

| Need | Start here | Source boundary |
| --- | --- | --- |
| Interactive approvals and session history | [Quickstart](quickstart.md) | QQ TUI and agent loop |
| Script or CI prompt | [Headless](headless.md) | QQ headless command and typed records |
| External tools | [MCP servers](mcp.md), then `qq mcp inspect NAME` | QQ MCP client, grants, and approvals |
| Native client integration | [Native modes](startup-native-modes.md) | Candidate HTTP/SSE clients; pairing remains unaccepted |
| Account-backed extraction | [API and billing](startup-api-billing.md) | Private Retsu Worker/DO/D1 source; hosted runtime unverified |
| Delivery planning | [Feature map](startup-feature-map.md) | Source, candidate, planned, and unverified labels |

The public CLI dispatch is in
[src/cli.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/cli.rs);
server routes are in
[crates/qq-server/src/lib.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-server/src/lib.rs).

## Report what happened

Record the exact source or binary revision, command, environment, expected
result, observed result, and remaining dependency. A compile check proves
compilation. A local provider response, native connection, hosted deployment,
payment event, and customer result each require their own evidence.
