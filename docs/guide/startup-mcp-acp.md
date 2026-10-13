# MCP and ACP

This page separates the protocol surfaces that are present in the reviewed QQ
source from development helpers and future compatibility work.

## QQ's MCP client

QQ's shipped runtime is the MCP client. The client implementation is in
[crates/qq-mcp/src/lib.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-mcp/src/lib.rs)
and the harness wires configured servers in
[crates/qq-harness/src/mcp.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/crates/qq-harness/src/mcp.rs).
The CLI exposes inspection through
[src/cli.rs](https://github.com/retsu-AI/qq/blob/9bae6937b50a013f652db8963ac3590103eb4b84/src/cli.rs):

```sh
qq mcp inspect NAME
```

Inspection contacts only the named server, reads descriptors, and does not
invoke a tool. QQ namespaces discovered tools as
`mcp__<server>__<tool>`. A declaration is still subject to project trust,
workspace grants, approval policy, descriptor limits, pinning, and quarantine.
A pin records the reviewed descriptor set; it does not guarantee the server's
future behavior. See the detailed [MCP servers](mcp.md) guide for stdio,
streamable HTTP, credentials, and grant configuration.

The `qq-development` MCP server helper is separate development tooling. It
may help a developer inspect or exercise a repository, but it is not QQ's
runtime client, does not grant QQ tools, and is not evidence of a hosted MCP
service. Candidate or private helper source requires repository access.

## ACP status

The reviewed QQ snapshot is `9bae6937b50a013f652db8963ac3590103eb4b84`
(origin/main). It contains no accepted ACP endpoint, adapter, or operator
workflow. Do not infer ACP compatibility from an unrelated editor or agent
project, including a Devin ACP implementation. ACP remains a candidate
compatibility surface until a source implementation, protocol identity,
conformance check, and runtime evidence are accepted.

## Startup boundary

An assistant can coordinate nodes and tool connections later, but it does not
replace QQ's MCP client or approval authority. The reviewed assistant and
native candidates do not turn this boundary into a currently hosted product.
Continue with [assistant and gateway](startup-assistant.md) and
[native modes](startup-native-modes.md) for the candidate boundary.
