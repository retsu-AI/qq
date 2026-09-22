# MCP servers

[Model Context Protocol](https://modelcontextprotocol.io) servers add tools
to the agent. QQ speaks stdio and streamable HTTP, discovers each server's
tools once, and exposes them as `mcp__<server>__<tool>` under the same
approval policy as built-ins.

## Declaring a server

```ron
(
    version: 1,
    mcp: {
        // A local process.
        "executor": Stdio(
            command: "executor",
            args: ["mcp"],
            env: ["EXECUTOR_API_KEY"],      // variables passed through from your shell
            eager: true,                    // connect at startup instead of on first use
            allow: ["execute", "skills"],   // tools that run without asking under `auto`
        ),

        // A remote endpoint.
        "linear": Http(
            url: "https://mcp.linear.app/mcp",
            bearer: Env("LINEAR_TOKEN"),    // or Stored("linear/default")
            call_timeout_seconds: 120,      // default 60
            max_concurrent_calls: 2,        // default 4
        ),

        // Drop one an earlier layer declared.
        "search": Remove,
    },
)
```

| Key | Stdio | Http | Meaning |
| --- | --- | --- | --- |
| `command`, `args` | required | | the process to spawn |
| `env` | optional | | names of environment variables the child inherits (default: none beyond a minimal set) |
| `url` | | required | the streamable-HTTP endpoint |
| `bearer` | | optional | `Env("NAME")` or `Stored("name")`; literal values are refused |
| `eager` | `false` | `false` | connect when QQ starts; otherwise on first use |
| `allow` | `[]` | `[]` | tool names granted for the workspace, folded into `policy.allow_tools` as `mcp__server__tool` |
| `call_timeout_seconds` | `60` | `60` | per call |
| `max_concurrent_calls` | `4` | `4` | per server |

Entries replace whole declarations by name; there is no per-field layering.

## Where to declare it

- **Your global config** for servers you use everywhere.
- **`.qq/config.ron`** for servers a repository needs. Because a stdio server
  is a command QQ will run, project MCP declarations require
  [`qq trust`](permissions.md#project-trust).
- **`.qq/config.d/50-local.ron`** (gitignored in this repository) for a
  server that needs *your* credential: `Stored("name")` resolves only on
  the machine where `qq auth set name` ran, so committing it breaks every
  other clone.
- **A pack** (`.qq/packs/<id>/pack.ron`) when the server belongs to an
  agent persona; a pack profile's `mcp: [...]` selects which of the pack's
  servers that profile may use.

## Credentials for HTTP servers

```sh
qq auth set linear/default --endpoint https://mcp.linear.app
```

then `bearer: Stored("linear/default")`. `--endpoint` binds the token to that
host. In CI prefer `bearer: Env("LINEAR_TOKEN")`.

## Approval

MCP calls are `External` effect class: under `read_only` they are denied
unless the server marks the tool read-only and it is allowlisted; under
`ask` each call asks; under `auto` and `full` they run. `allow: [...]` on the
server is the workspace grant; `--allow-tool mcp__linear__create_issue`
grants one for a headless run; `a` in the TUI grants one for the session.

## When a server is unavailable

A server that fails to start or connect contributes no tools; the tool
catalog reports `unavailable MCP servers: NAME` and the run continues with
built-ins and the other servers. Calls to it return a typed unavailable
error to the model. The next use retries with backoff.

A server whose `Stored(...)` bearer is not in this machine's credential store
currently fails the run with ``credential `NAME` is not registered``; making
that degrade like a connection failure is planned
([`../plans/onboarding-ux.md`](../plans/onboarding-ux.md) OB9). Until then,
register the credential or move the declaration to a local fragment.

## Inspecting

- `qq config show` lists declared servers with redacted bearers.
- In the TUI, an MCP tool call row shows the server, tool, and arguments
  when expanded (`Enter` on the row).
- `qq config explain grant.tool.mcp__linear__create_issue` says which layer
  granted a tool.

Bounds and the host model are in [`../design/tools.md`](../design/tools.md)
§ External Tool Hosts and § MCP.
