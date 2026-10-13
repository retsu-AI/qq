# MCP and ACP

QQ currently implements Model Context Protocol support. ACP remains a separate
compatibility question until an accepted source implementation is identified.

## MCP in QQ

Declare an MCP server in QQ configuration, inspect its descriptors, and pin
the reviewed tool-set digest when the server is stable. QQ namespaces tools as
`mcp__<server>__<tool>`. The MCP guide explains stdio and streamable HTTP,
credentials, grants, descriptor limits, quarantine, and unavailable servers.

Use [MCP servers](mcp.md) as the detailed reference. It is the source of truth
for user configuration. `qq mcp inspect NAME` contacts only the named server,
calls no tool, and prints descriptors and the digest.

MCP calls still pass through QQ's approval policy. A declaration does not
grant a tool. A pin covers advertised descriptors, not every behavior of the
server. An unavailable or changed server remains unavailable until its current
state is reviewed.

## ACP status

The current QQ checkout has no accepted ACP implementation or public ACP
operator guide. Do not document an ACP endpoint, adapter, or compatibility
claim as available. The startup requirement records ACP as planned research or
future implementation until source, protocol identity, checks, and runtime
evidence exist.

## Assistant boundary

The assistant may eventually coordinate nodes and tool connections, but it does
not replace QQ's tool and approval authority. See [Assistant gateway and nodes](startup-assistant.md)
for the current candidate boundary.
