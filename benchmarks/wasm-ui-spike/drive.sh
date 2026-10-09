#!/usr/bin/env bash
# Creates `--sessions` sessions in a workspace and submits one prompt to each
# so the spike pages have something to stream. Reads the server token from the
# user's runtime metadata (never from a URL).
#
#   ./drive.sh http://127.0.0.1:9077 /path/to/workspace 3
set -euo pipefail
server="${1:?server base url}"
workspace="${2:?workspace path}"
sessions="${3:-1}"
model="${QQ_SPIKE_MODEL:-fake/stream}"
token="$(python3 -c 'import re,sys;print(re.search(r"token:\"([^\"]+)\"",open(sys.argv[1]).read()).group(1))' "${QQ_SERVER_RON:-$HOME/.local/share/qq/runtime/server.ron}")"

post() {
  curl -sS -H "Authorization: Bearer $token" -H "content-type: application/json" \
    -d "$2" "$server$1"
}
cid() { python3 -c 'import uuid;print(uuid.uuid4().hex)'; }

workspace_id="$(post /v1/workspaces/resolve "{\"command_id\":\"$(cid)\",\"command\":{\"type\":\"resolve_workspace\",\"path\":\"$workspace\"}}" | python3 -c 'import json,sys;print(json.load(sys.stdin)["outcome"]["workspace_id"])')"
echo "workspace $workspace_id"
for i in $(seq 1 "$sessions"); do
  session_id="$(post /v1/sessions "{\"command_id\":\"$(cid)\",\"command\":{\"type\":\"create_session\",\"workspace_id\":\"$workspace_id\",\"model\":{\"model\":\"$model\"},\"approval_mode\":\"auto\"}}" | python3 -c 'import json,sys;print(json.load(sys.stdin)["outcome"]["session_id"])')"
  post /v1/sessions/prompts "{\"command_id\":\"$(cid)\",\"command\":{\"type\":\"submit_prompt\",\"session_id\":\"$session_id\",\"input\":[{\"type\":\"text\",\"text\":\"stream test $i\"}]}}" >/dev/null
  echo "session $session_id prompted"
done
