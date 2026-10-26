# MCP

## Sub-features

- Config: `mcp: {"name": Stdio(command, args, env, eager, allow, pin)}` or
  `Http(url, bearer, …)` (`docs/guide/mcp.md`).
- `qq mcp inspect NAME` prints JSON with `server`, `digest`,
  `configured_pin`, `matches_pin`, `warning`, and `tools`.
- Tools are exposed as `mcp__<server>__<tool>` and classed External.

## How to get to it (user POV)

Declare a server in the global config, or in `.qq/config.ron`, which needs
`qq trust`. Then run `qq mcp inspect NAME`.

## Driving it headlessly

- Failure paths verified:
  - `qq mcp inspect nope` exits 1 with
    `MCP server "nope" is not configured`.
  - A bad command exits 1 with `could not start MCP server`.
- Success path: there is no standalone stdio MCP fixture yet. The
  in-process fixtures are `crates/qq-mcp/src/tests.rs` and
  `crates/qq-core/tests/mcp_session.rs`. To drive the binary, write a small
  stdio JSON-RPC server script under `$QQ_HOME` and add it to the config.
  Adding one to `xtask/src/smoke.rs` is the obvious next smoke scenario.

## Gotchas

- MCP calls are denied under `read-only`, held under `ask`, and run under `auto` and `full`.
- MCP is not available under `--tui-qa-root`.
- `inspect` gives up after 45 s.
