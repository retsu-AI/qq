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

Verified end to end:

```sh
H=.qq/skills/verify-qq/qq-home.sh
eval "$($H new <name>)"; $H serve <name>
TOKEN=$(sed -n 's/.*token:"\([0-9a-f]*\)".*/\1/p' $QQ_HOME/runtime/qq/server.ron)
ADDR=$(sed -n 's/.*address:"\([^"]*\)".*/\1/p' $QQ_HOME/runtime/qq/server.ron)
api() { curl -s -H "Authorization: Bearer $TOKEN" -H 'content-type: application/json' "http://$ADDR$1" -d "$2"; }
cid() { openssl rand -hex 16; }

WS=$(api /v1/workspaces/resolve "{\"command_id\":\"$(cid)\",\"command\":{\"type\":\"resolve_workspace\",\"path\":\"$QQ_HOME/workspace\"}}" | jq -r .outcome.workspace_id)
# Last-Event-ID is the string store_id:workspace_id:sequence, not the JSON object.
CUR=$(api /v1/workspaces/snapshot "{\"workspace_id\":\"$WS\",\"session_limit\":8,\"message_limit\":8}" \
  | jq -r '.cursor|"\(.store_id):\(.workspace_id):\(.sequence)"')
curl -sN -m 10 -H "Authorization: Bearer $TOKEN" -H 'Accept: text/event-stream' -H "Last-Event-ID: $CUR" \
  http://$ADDR/v1/workspaces/$WS/events > $QQ_HOME/evidence/sse.txt &
SID=$(api /v1/sessions "{\"command_id\":\"$(cid)\",\"command\":{\"type\":\"create_session\",\"workspace_id\":\"$WS\",\"model\":{\"model\":\"verify/model\"}}}" | jq -r .outcome.session_id)
api /v1/sessions/prompts "{\"command_id\":\"$(cid)\",\"command\":{\"type\":\"submit_prompt\",\"session_id\":\"$SID\",\"input\":[{\"type\":\"text\",\"text\":\"hi\"}]}}" | jq -c .outcome
sleep 3; rg -o '"type":"(session_created|text_appended|run_finished)"' $QQ_HOME/evidence/sse.txt
```

- SSE frames: `id: <cursor>`, `event: session_event`, `data: <envelope>`.
- Resume: reconnect with the last `id`. Only events with a higher sequence
  come back.
- The smoke scenario `serve-sse` covers the same flow through
  `qq_client::SessionClient`, including the reconnect.

## Gotchas

- A request without the bearer token gets 401
  `{"error":"authentication required"}`.
- `GET …/events` without `Last-Event-ID` gets 400 `Last-Event-ID is required`.
- The server binds only to loopback. Keep-alive comments arrive every 15 s.
- `qq run` exits 4 while this home's server is running. Stop the server first.
