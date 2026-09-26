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
            pin: "5a1f…e9c0",               // refuse the server if its tool set changes
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
| `pin` | optional | optional | the 64-hex-digit digest of the server's tool set from `qq mcp inspect`; a server whose tools no longer match is quarantined |

Entries replace whole declarations by name; there is no per-field layering.

## Pinning a server's tools

An MCP server can change what its tools are called, what they accept, and
what they say they do at any time, and the model reads those descriptions as
instructions. `pin` freezes the tool set you reviewed. QQ reduces every
listing to one SHA-256 digest over each tool's namespaced name, description,
input schema, and hints (listing order does not matter) and, when a pinned
server lists anything else, quarantines it: its tools leave the catalog,
every call to it is refused — including tools that did not change themselves,
because a server that changed one tool cannot be trusted about the others —
and the readiness message names the server with both digests:

```text
quarantined MCP servers: linear (tool set digests to 9c2e… but the configured pin is 5a1f…)
```

To pin a server, inspect it, review what it declares, and copy the digest:

```sh
qq mcp inspect linear
```

connects to that one server (it starts the configured process or contacts
the endpoint), calls no tool, and prints a JSON report: `digest`,
`configured_pin` and `matches_pin` (null when no pin is set), and every tool's
`name`, `description`, `input_schema`, and `hints`. Read the descriptions and
schemas as untrusted text — that is exactly what the model will read — then
put `digest` into `pin`. When a server legitimately changes, run the command
again, review the new descriptors, and update the pin. A server that returns
to the pinned listing leaves quarantine on its next `list_changed`
notification without a restart.

A pin covers what the server *advertises*, not what its code does: a server
can keep its descriptors identical and change its behavior. Pins are also
enforced at dispatch, not only at discovery. A call that was queued behind
the server's concurrency bound is re-checked against the listing it was
admitted under before the request is sent, so a `list_changed` notification
or a reconnect that arrives while the call waits refuses it instead of
letting it run against a tool set nobody reviewed; a pinned call to a tool
absent from the listing is refused as unknown. A pinned server's listing is
also taken whole or not at all: a listing with a malformed or duplicate tool
name, more than 512 tools, more than 1 MiB of descriptors, or more than 32
pages is unavailable rather than partially pinned. The pin is part of the
compiled plan's identity, so a run records which tool set it was admitted
against and changing a pin recompiles the plan. Unpinned servers behave as
before.

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
catalog reports `unavailable MCP servers: NAME (reason)` and the run
continues with built-ins and the other servers. Calls to it return a typed
unavailable error to the model. The next use retries with backoff.

A server whose `Stored(...)` or `Env(...)` bearer does not resolve on this
machine degrades the same way rather than failing the run. Its `allow`
grants stay in force and its tool names stay reserved; the readiness
message names the credential and the fix:

```text
unavailable MCP servers: linear (credential `linear/default` is not registered; run `qq auth set linear/default`)
```

Run the command named, and the next run picks the credential up without a
restart: the tool registry is keyed by the credential store's epoch, which
`qq auth set` advances. `qq doctor` reports the same finding under `mcp`
as a warning. The TUI shows the same reason as a warning on the composer
rule when it connects (once per distinct message) and under the search row
of `/skills`. Inline `Value(...)` bearers are unaffected.

## Inspecting

- `qq mcp inspect NAME` connects to one server and prints its tool
  descriptors and digest as JSON without calling anything.
- `qq config show` lists declared servers with redacted bearers.
- In the TUI, an MCP tool call row shows the server, tool, and arguments
  when expanded (`Enter` on the row).
- `qq config explain grant.tool.mcp__linear__create_issue` says which layer
  granted a tool.

Bounds and the host model are in [`../design/tools.md`](../design/tools.md)
§ External Tool Hosts and § MCP.
