#!/usr/bin/env bash
# Isolated qq home plus a scripted fake OpenAI Responses endpoint.
# See .qq/skills/verify-qq/SKILL.md. Usage:
#   qq-home.sh new <name>          create target/qq-verify/<name>, start the fake; prints env
#   qq-home.sh qq <name> ARGS...   run target/debug/qq inside the home
#                                  (QQ_CONTEXT_WINDOW=N sets the model's window; default 200000)
#   qq-home.sh serve <name>        start `qq serve` in the background; prints its address
#   qq-home.sh stop <name>         stop the fake and any serve this home started
#   qq-home.sh rm <name>           stop, then delete the home
set -euo pipefail

repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
# Homes live in this checkout even when CARGO_TARGET_DIR is shared.
target=$repo/target
qq_bin=${QQ_BIN:-${CARGO_TARGET_DIR:-$repo/target}/debug/qq}

die() { echo "qq-home: $*" >&2; exit 1; }
home_of() { [[ $1 =~ ^[a-z0-9][a-z0-9-]*$ ]] || die "name must be kebab-case: $1"; echo "$target/qq-verify/$1"; }

qq_env() {
  local home=$1 port
  shift
  port=$(cat "$home/port")
  local config
  config=$(printf '(version: 1, model: "verify/model", providers: {"verify": Custom(connection: (base_url: "http://127.0.0.1:%s/v1", api: OpenAiResponses, auth: NoAuth), models: {"model": (name: "Verify model", context_window: %s)})})' "$port" "${QQ_CONTEXT_WINDOW:-200000}")
  exec env -i PATH="$PATH" TERM="${TERM:-xterm-256color}" NO_COLOR=1 \
    HOME="$home" XDG_CONFIG_HOME="$home/config" XDG_DATA_HOME="$home/data" \
    XDG_STATE_HOME="$home/state" XDG_RUNTIME_DIR="$home/runtime" \
    QQ_CONFIG_CONTENT="$config" "$@"
}

fake_server() {
  exec python3 - "$1" "$2" <<'PY'
import json, sys, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
home, port = sys.argv[1], int(sys.argv[2])
# The reply index restarts whenever script.jsonl changes, so each scripted
# scenario starts from its first reply.
lock = threading.Lock(); served = [0]; script_mtime = [None]
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
            import os
            try:
                mtime = os.stat(f"{home}/script.jsonl").st_mtime_ns
                script = [json.loads(l) for l in open(f"{home}/script.jsonl") if l.strip()]
            except FileNotFoundError:
                mtime, script = None, []
            if mtime != script_mtime[0]:
                script_mtime[0] = mtime; served[0] = 0
            n = served[0]; served[0] += 1
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

# Linux pidfds bind a signal to a process instance, not a reusable PID.
# Elsewhere fail closed instead of risking another agent's process.
process_record() {
  python3 - "$@" <<'PY'
import os, signal, sys
mode, home = sys.argv[1:3]
def identity(pid):
    try:
        return open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()[19]
    except (FileNotFoundError, ProcessLookupError):
        return None
if mode == "add":
    pid = int(sys.argv[3])
    start = identity(pid)
    if start is None:
        sys.exit("qq-home: cannot identify child; automatic cleanup requires Linux /proc")
    with open(f"{home}/pids", "a") as f:
        f.write(f"{pid} {start}\n")
else:
    try:
        records = open(f"{home}/pids").readlines()
    except FileNotFoundError:
        records = []
    for record in records:
        fields = record.split()
        if len(fields) != 2 or not all(s.isdecimal() for s in fields):
            continue  # Old PID-only records are never safe to signal.
        pid, start = fields
        if int(pid) <= 1:
            continue
        if mode == "live":
            if identity(pid) == start:
                print(pid)
            continue
        try:
            fd = os.pidfd_open(int(pid))
        except ProcessLookupError:
            continue
        except (AttributeError, OSError) as e:
            sys.exit(f"qq-home: safe automatic cleanup unavailable: {e}; stop the child manually")
        try:
            if identity(pid) == start:
                signal.pidfd_send_signal(fd, signal.SIGTERM)
        except ProcessLookupError:
            pass
        finally:
            os.close(fd)
PY
}

live_pids() { process_record live "$1"; }

stop_home() {
  local home=$1
  [[ -n $home ]] || die "no home"
  process_record stop "$home"
  rm -f "$home/pids"
}

cmd=${1:-}; shift || true
case $cmd in
  new)
    home=$(home_of "${1:?name}")
    # Another agent may be using this name; never stop what we did not start.
    [[ -z $(live_pids "$home") ]] || die "home $1 is in use (live processes in $home/pids); pick another name or run: $0 stop $1"
    rm -rf "$home"
    mkdir -p "$home"/{config,data,state,runtime,workspace,evidence}
    chmod 700 "$home/runtime"
    printf 'pub struct Widget;\npub struct Gadget;\n' > "$home/workspace/inventory.rs"
    git -C "$home/workspace" init -q
    echo '{"text": "verify fake reply"}' > "$home/script.jsonl"
    port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
    echo "$port" > "$home/port"
    fake_server "$home" "$port" >"$home/fake.log" 2>&1 &
    process_record add "$home" "$!"
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
    [[ -x $qq_bin ]] || die "$qq_bin missing; run cargo build --bin qq"
    cd "$home/workspace"
    qq_env "$home" "$qq_bin" serve --bind 127.0.0.1:0 >"$home/serve.log" 2>&1 &
    process_record add "$home" "$!"
    for _ in $(seq 100); do grep -q 'listening at' "$home/serve.log" 2>/dev/null && break; sleep 0.1; done
    grep 'listening at' "$home/serve.log" || die "qq serve did not start; see $home/serve.log"
    echo "discovery: $home/runtime/qq/server.ron"
    ;;
  stop) home=$(home_of "${1:?name}"); stop_home "$home" ;;
  rm) home=$(home_of "${1:?name}"); stop_home "$home"; rm -rf "$home" ;;
  *) sed -n '2,9p' "$0"; exit 2 ;;
esac
