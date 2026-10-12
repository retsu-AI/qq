# serve and SSE

## Sub-features

- `qq serve [--bind 127.0.0.1:0] [--allow-origin ORIGIN]…`. The first line on
  stdout is `qq server listening at ADDR`.
- Discovery file: `$XDG_RUNTIME_DIR/qq/server.ron`, next to `server.lock`. It
  holds the address, pid, protocol version, and the bearer `token`.
- Routes (`crates/qq-server/src/lib.rs`, `SessionCommandKind::route`):
  - `GET /v1/health`
  - `GET /v1/workspaces/{id}/events` (SSE)
  - `POST /v1/workspaces/resolve`, `/v1/workspaces/snapshot`, `/v1/capabilities`, `/v1/models`
  - `POST /v1/sessions`, `/v1/sessions/prompts`
  - `POST /v1/runs/steer`, `/v1/runs/cancel`
  - `POST /v1/tools/approvals`
  - `POST /v1/sessions/{approval-mode, approval-delegate, model, profile, effort, delete, prune, compact, compact/rollback, side-questions, side-questions/cancel}`

## How to get to it (user POV)

Run `qq serve` in one terminal. A `qq` TUI then attaches to it. Stopping the
server cancels its runs.

## Driving it headlessly

```sh
.qq/skills/verify-qq/qq-home.sh serve <name>
TOKEN=$(sed -n 's/.*token:"\([0-9a-f]*\)".*/\1/p' $QQ_HOME/runtime/qq/server.ron)
ADDR=$(sed -n 's/.*address:"\([^"]*\)".*/\1/p' $QQ_HOME/runtime/qq/server.ron)
curl -s -H "Authorization: Bearer $TOKEN" http://$ADDR/v1/health
```

Steps for a full run:

1. `POST /v1/workspaces/resolve` with
   `{"command_id":"<32hex>","command":{"type":"resolve_workspace","path":"$QQ_HOME/workspace"}}`.
2. `POST /v1/workspaces/snapshot` with
   `{"workspace_id","session_limit":8,"message_limit":8}`. Read `cursor`
   from the response.
3. `GET /v1/workspaces/<id>/events` with `Last-Event-ID: <cursor>` (the
   header is required) and `Accept: text/event-stream`.
4. `POST /v1/sessions` with `create_session` and
   `"model":{"model":"verify/model"}`.
5. `POST /v1/sessions/prompts` with `submit_prompt` and
   `input:[{"type":"text","text":"hi"}]`.

SSE frames are `id: <cursor>`, `event: session_event`, `data: <envelope>`.
To resume, reconnect with the last `id`; only events with a higher sequence
come back. The smoke scenario `serve-sse` drives this sequence through
`qq_client::SessionClient`.

## Gotchas

- A request without the bearer token gets 401
  `{"error":"authentication required"}`.
- The server binds only to loopback. Keep-alive comments arrive every 15 s.
- `qq run` exits 4 while this home's server is running. Stop the server first.
