# Get started with the stack

Set up QQ first. Then choose the smallest workflow that matches your task.

## Set up QQ

Follow [Install](install.md), then follow [Quickstart](quickstart.md) to store
a provider credential, select a model, run `qq doctor`, and open a first
session. The existing pages contain the complete commands and platform paths.

For a repository with sensitive project configuration, review the file before
running `qq trust`. Project configuration can declare providers, MCP servers,
models, and grants. QQ does not load those sensitive sections until you trust
the project. See [Permissions and trust](permissions.md).

## Pick a workflow

Use the TUI when you want interactive approvals, a persistent session, and
visible tool calls. Use [headless commands](headless.md) when a script or CI
job owns the prompt. Use `qq serve` when several clients need one long-running
local server.

Use [MCP servers](mcp.md) when an external tool host is needed. Inspect its
descriptors before pinning them. MCP does not grant a tool permission by
itself. Approval policy and workspace grants still apply.

The native client sources use QQ's HTTP and SSE contract. They do not replace
the QQ agent loop. The assistant source adds a private store around QQ; it does
not turn a local source candidate into a hosted service.

## Check what is true

Before reporting a workflow as complete, identify its subject and evidence.
`cargo build` proves compilation for that source. A local executable journey
proves only the exercised local path. A provider response, a device session,
a deployment, and a customer result require their own evidence.

Use [Recovery](startup-recovery.md) when a command times out, a session needs
reopening, or a source candidate needs an explicit adoption decision.
