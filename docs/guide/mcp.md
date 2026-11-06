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
            allow: ["execute", "skills"],   // tools `qq ask` may call
        ),

        // A remote endpoint.
        "linear": Http(
            url: "https://mcp.linear.app/mcp",
            bearer: Env("LINEAR_TOKEN"),    // or Stored("linear/default")
            call_timeout_seconds: 120,      // default 60
            max_concurrent_calls: 2,        // default 4
            // Refuse the server if its tool set changes; `qq mcp inspect`
            // prints the digest to paste here.
            pin: "5a1f0c3d9e8b7a6f5e4d3c2b1a0f9e8d7c6b5a4f3e2d1c0b9a8f7e6d5c4be9c0",
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
| `allow` | `[]` | `[]` | tool names `qq ask` may call, which has no approval channel; [Approval](#approval) |
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
against and changing a pin recompiles the plan.

## A tool that changes during a run

Every run keeps the tool declarations it was admitted with, pinned or not.
If a server's `tools/list` changes while a run is in progress, the model keeps
seeing the original schemas, and each call is checked against the server's
current listing before it is sent. A tool whose name, description, schema, or
hints changed, or that the server no longer lists, is refused and never
reaches the server:

```text
mcp__linear__create_issue changed its declaration after this run was admitted; the run keeps the contract it was admitted with, so the call was not sent. Start a new run to use the new declaration
```

The next run compiles against the new listing. On an unpinned server that
means the changed declaration is accepted the same way a new server is. On
a pinned server it means quarantine. The check depends on the server's
`list_changed` notification. A server that changes a tool without sending
one, or changes it in the instant between the check and the send, is caught
only on a pinned server, where the send itself is gated on the pinned
listing.

## Where to declare it

- **Your global config** for servers you use everywhere.
- **`.qq/config.ron`** for servers a repository needs. Because a stdio server
  is a command QQ will run, project MCP declarations require
  [`qq trust`](permissions.md#project-trust).
- **`.qq/config.d/50-local.ron`** (gitignored in this repository) for a
  server that needs *your* credential: `Stored("name")` resolves only on
  the machine where `qq auth set name` ran, so committing it breaks every
  other clone.

## Credentials for HTTP servers

```sh
qq auth set linear/default --endpoint https://mcp.linear.app
```

then `bearer: Stored("linear/default")`. `--endpoint` binds the token to that
host. In CI prefer `bearer: Env("LINEAR_TOKEN")`.

## Approval

MCP calls are `External` effect class: under `read_only` they are denied;
under `ask` each call asks; under `auto` and `full` they run without asking.
`qq ask` cannot ask, so it offers only the tools listed in the server's
`allow: [...]` and runs them without asking.

## When a server is unavailable

Every declared server is in one of these states. `qq mcp status` prints the
state and exact reason for each one:

| State | Meaning | Its tools |
| --- | --- | --- |
| `ready` | listed its tools (and they match the pin, if one is set) | declared to the model, directly or through `select_tools` |
| `needs_auth` | its bearer did not resolve on this machine, or the server answered HTTP 401/403 with a `WWW-Authenticate` challenge | none; calls return the reason |
| `unavailable` | it failed to start, connect, or list tools within bounds | none; calls return the reason |
| `quarantined` | its listing no longer matches `pin` | none; every call is refused |

```sh
qq mcp status
```

connects to every declared server, calls no tool, and prints JSON with
`server`, `state`, `reason`, `configured_pin`, `digest`, and, for `ready`
servers, the tool names. Tool descriptions are not printed. Use
`qq mcp inspect NAME` to review them as untrusted text.

A server that fails to start or connect contributes no tools; the tool
catalog reports `unavailable MCP servers: NAME (reason)` and the run
continues with built-ins and the other servers. Calls to it return a typed
unavailable error to the model. The next use retries with backoff.

A server whose `Stored(...)` or `Env(...)` bearer does not resolve on this
machine, or that rejects the bearer with an HTTP 401 or 403 challenge (a token that
expired or was revoked), is reported separately, so you know what to fix.
Its `allow` list stays in force and its tool names stay reserved; the
readiness message names the credential and the fix:

```text
MCP servers need authorization: linear (credential `linear/default` is not registered; run `qq auth set linear/default`)
```

Run the command named, and the next run picks the credential up without a
restart: the tool registry is keyed by the credential store's epoch, which
`qq auth set` advances. QQ does not run an OAuth flow for MCP servers. Get
the token from the server's own login flow and store it with
`qq auth set NAME --endpoint URL`, which keeps it in the OS keyring. `qq doctor` reports the same finding under `mcp`
as a warning. The TUI shows the same reason as a warning on the composer
rule when it connects (once per distinct message) and under the search row
of `/skills`. Inline `Value(...)` bearers are unaffected.

## Inspecting

- `qq mcp status` connects to every declared server and prints each one's
  state and reason as JSON without calling anything.
- `qq mcp inspect NAME` connects to one server and prints its tool
  descriptors and digest as JSON without calling anything.
- `qq config show` lists declared servers with redacted bearers.
- In the TUI, an MCP tool call row shows the server, tool, and arguments
  when expanded (`Enter` on the row).

Bounds and the host model are in [`../design/tools.md`](../design/tools.md)
§ External Tool Hosts and § MCP.
