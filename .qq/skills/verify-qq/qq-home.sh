#!/usr/bin/env bash
# Isolated qq home plus a scripted fake OpenAI Responses endpoint.
# See .qq/skills/verify-qq/SKILL.md. Usage:
#   qq-home.sh new <name>          create target/qq-verify/<name>, start the fake; prints env
#   qq-home.sh qq <name> ARGS...   run target/debug/qq inside the home
#   qq-home.sh serve <name>        start `qq serve` in the background; prints its address
#   qq-home.sh stop <name>         stop the fake and any serve this home started
#   qq-home.sh rm <name>           stop, then delete the home
set -euo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
target=${CARGO_TARGET_DIR:-$repo/target}
qq_bin=${QQ_BIN:-$target/debug/qq}

die() { echo "qq-home: $*" >&2; exit 1; }
home_of() { [[ $1 =~ ^[a-z0-9][a-z0-9-]*$ ]] || die "name must be kebab-case: $1"; echo "$target/qq-verify/$1"; }

qq_env() {
  local home=$1 port
  shift
  port=$(cat "$home/port")
  local config
  config=$(printf '(version: 1, model: "verify/model", providers: {"verify": Custom(connection: (base_url: "http://127.0.0.1:%s/v1", api: OpenAiResponses, auth: NoAuth), models: {"model": (name: "Verify model", context_window: 200000)})})' "$port")
  env -i PATH="$PATH" TERM="${TERM:-xterm-256color}" NO_COLOR=1 \
    HOME="$home" XDG_CONFIG_HOME="$home/config" XDG_DATA_HOME="$home/data" \
    XDG_STATE_HOME="$home/state" XDG_RUNTIME_DIR="$home/runtime" \
    QQ_CONFIG_CONTENT="$config" "$@"
}

fake_server() {
  exec python3 - "$1" "$2" <<'PY'
import json, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
home, port = sys.argv[1], int(sys.argv[2])
lock = threading.Lock(); served = [0]
done = {"type": "response.completed", "response": {"usage": {"input_tokens": 3, "output_tokens": 2}}}
def sse(events):
    return "".join(f"data: {json.dumps(e)}\n\n" for e in events).encode()
def reply(spec):
    if "status" in spec:
        return spec["status"], "application/json", spec.get("body", "{}").encode()
    if "tool" in spec:
        n = served[0]
        item = {"type": "function_call", "id": f"fc_{n}", "call_id": f"call_{n}", "name": spec["tool"],
                "arguments": json.dumps(spec.get("args", {}))}
        start = dict(item, arguments="")
        return 200, "text/event-stream", sse([
            {"type": "response.output_item.added", "output_index": 0, "item": start},
            {"type": "response.output_item.done", "output_index": 0, "item": item}, done])
    return 200, "text/event-stream", sse([{"type": "response.output_text.delta", "delta": spec.get("text", "")}, done])
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b"ok"; self.send_response(200); self.send_header("content-length", "2"); self.end_headers(); self.wfile.write(body)
    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", "0")))
        with lock:
            n = served[0]; served[0] += 1
            try:
                script = [json.loads(l) for l in open(f"{home}/script.jsonl") if l.strip()]
            except FileNotFoundError:
                script = []
            spec = script[min(n, len(script) - 1)] if script else {"text": "(no script.jsonl)"}
            with open(f"{home}/provider-requests.txt", "a") as log:
                log.write(f"----- request {n} {self.path} -----\n{body.decode(errors='replace')}\n\n")
        status, ctype, payload = reply(spec)
        self.send_response(status); self.send_header("content-type", ctype)
        self.send_header("content-length", str(len(payload))); self.end_headers(); self.wfile.write(payload)
    def log_message(self, *a): pass
ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
PY
}

stop_home() {
  local home=$1
  [[ -f $home/pids ]] || return 0
  while read -r pid; do kill "$pid" 2>/dev/null || true; done < "$home/pids"
  rm -f "$home/pids"
}

cmd=${1:-}; shift || true
case $cmd in
  new)
    home=$(home_of "${1:?name}")
    stop_home "$home"; rm -rf "$home"
    mkdir -p "$home"/{config,data,state,runtime,workspace,evidence}
    chmod 700 "$home/runtime"
    printf 'pub struct Widget;\npub struct Gadget;\n' > "$home/workspace/inventory.rs"
    git -C "$home/workspace" init -q
    [[ -f $home/script.jsonl ]] || echo '{"text": "verify fake reply"}' > "$home/script.jsonl"
    port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
    echo "$port" > "$home/port"
    fake_server "$home" "$port" >"$home/fake.log" 2>&1 &
    echo $! >> "$home/pids"
    for _ in $(seq 50); do curl -sf "http://127.0.0.1:$port/health" >/dev/null && break; sleep 0.1; done
    curl -sf "http://127.0.0.1:$port/health" >/dev/null || die "fake did not start; see $home/fake.log"
    printf 'QQ_HOME=%q\nQQ_FAKE_PORT=%q\nexport QQ_HOME QQ_FAKE_PORT\n' "$home" "$port"
    ;;
  qq)
    home=$(home_of "${1:?name}"); shift
    [[ -x $qq_bin ]] || die "$qq_bin missing; run cargo build --bin qq"
    cd "$home/workspace" && qq_env "$home" "$qq_bin" "$@"
    ;;
  serve)
    home=$(home_of "${1:?name}")
    cd "$home/workspace"
    qq_env "$home" "$qq_bin" serve --bind 127.0.0.1:0 >"$home/serve.log" 2>&1 &
    echo $! >> "$home/pids"
    for _ in $(seq 100); do grep -q 'listening at' "$home/serve.log" 2>/dev/null && break; sleep 0.1; done
    grep 'listening at' "$home/serve.log" || die "qq serve did not start; see $home/serve.log"
    echo "discovery: $home/runtime/qq/server.ron"
    ;;
  stop) stop_home "$(home_of "${1:?name}")" ;;
  rm) home=$(home_of "${1:?name}"); stop_home "$home"; rm -rf "$home" ;;
  *) sed -n '2,9p' "$0"; exit 2 ;;
esac
