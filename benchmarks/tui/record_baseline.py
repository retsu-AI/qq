"""Record the v0.1.11 B1 TUI baseline against the real `qq` binary in a pty.

usage: record_baseline.py --qq target/release/qq --out target/qq-perf/v0.1.11 \
           [--samples 20] [--keystrokes 60] [--qq-cpu 2] [--self-cpu 3]

Every launch runs in an isolated home (HOME and XDG_* under a fresh temporary
directory, configuration through QQ_CONFIG_CONTENT, as in
.qq/skills/verify-qq/qq-home.sh) or in a fresh --tui-qa-root fixture
(docs/runbooks/tui-qa.md). The provider is an in-process loopback fake. The
user's QQ configuration and data are never read. Only processes this script
started are signalled. Writes recorder.json (raw samples and summaries) to --out.
"""

import argparse
import json
import os
import platform
import pty
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import fcntl
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

COLUMNS, ROWS = 160, 40
PLACEHOLDER = "Ask QQ"
CSI = re.compile(rb"\x1b\[[0-9;?<>=]*[ -/]*[@-~]|\x1b[()][0-9A-Za-z]|\x1b[=>78]")
MARK = re.compile(r"mark(\d+)")
STREAM_TEXT = "words keep flowing in small lowercase pieces "
DRAFT = "QXZJVKWYPQZXJKVWYQPJXZKV"
TIMEOUT = 20.0


def done_event():
    return {"type": "response.completed", "response": {"usage": {"input_tokens": 3, "output_tokens": 2}}}


def sse(event):
    return f"data: {json.dumps(event)}\n\n".encode()


class Fake(BaseHTTPRequestHandler):
    stream_seconds = 40.0
    delta_interval = 0.02
    active = [0]
    lock = threading.Lock()

    def do_POST(self):
        body = self.rfile.read(int(self.headers.get("content-length", "0"))).decode(errors="replace")
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        try:
            if "STREAM" in body:
                with self.lock:
                    self.active[0] += 1
                try:
                    deadline = time.monotonic() + self.stream_seconds
                    found = re.search(r"STREAM (\d+)", body)
                    stream = found.group(1) if found else "0"
                    count = 0
                    while time.monotonic() < deadline:
                        count += 1
                        delta = f"{STREAM_TEXT}flow{stream}x{count} "
                        self.wfile.write(sse({"type": "response.output_text.delta", "delta": delta}))
                        self.wfile.flush()
                        time.sleep(self.delta_interval)
                finally:
                    with self.lock:
                        self.active[0] -= 1
            else:
                found = MARK.search(body)
                text = f"ANS{found.group(1)}ANS settled." if found else "B1 fake reply."
                self.wfile.write(sse({"type": "response.output_text.delta", "delta": text}))
            self.wfile.write(sse(done_event()))
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

    def log_message(self, *_args):
        pass


def config_ron(port):
    return (
        '(version: 1, model: "custom/qa-model", providers: {"custom": Custom(connection: '
        f'(base_url: "http://127.0.0.1:{port}/v1", api: OpenAiResponses, auth: NoAuth), '
        'models: {"qa-model": (name: "QA")})})'
    )


class Home:
    def __init__(self, scratch, port, name):
        self.root = os.path.join(scratch, name)
        for part in ("config", "data", "state", "runtime", "workspace"):
            os.makedirs(os.path.join(self.root, part))
        os.chmod(os.path.join(self.root, "runtime"), 0o700)
        subprocess.run(["git", "init", "-q", os.path.join(self.root, "workspace")], check=True,
                       env={"PATH": os.environ["PATH"], "HOME": self.root, "GIT_CONFIG_GLOBAL": "/dev/null",
                            "GIT_CONFIG_NOSYSTEM": "1"})
        self.env = {
            "PATH": os.environ["PATH"], "TERM": "xterm-256color", "HOME": self.root,
            "XDG_CONFIG_HOME": f"{self.root}/config", "XDG_DATA_HOME": f"{self.root}/data",
            "XDG_STATE_HOME": f"{self.root}/state", "XDG_RUNTIME_DIR": f"{self.root}/runtime",
            "QQ_CONFIG_CONTENT": config_ron(port),
        }
        self.cwd = os.path.join(self.root, "workspace")


class Tty:
    def __init__(self, argv, env, cwd):
        master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLUMNS, 0, 0))
        self.master = master
        self.raw = bytearray()
        self.started = time.monotonic()
        self.process = subprocess.Popen(argv, stdin=slave, stdout=slave, stderr=slave, env=env, cwd=cwd,
                                        start_new_session=True)
        os.close(slave)

    def mark(self):
        return len(self.raw)

    def pump(self, timeout):
        ready, _, _ = select.select([self.master], [], [], timeout)
        if not ready:
            return False
        try:
            chunk = os.read(self.master, 1 << 16)
        except OSError:
            chunk = b""
        if not chunk:
            raise RuntimeError("qq closed the terminal: " + text(self.raw[-400:]))
        self.raw += chunk
        return True

    def wait_for(self, needle, since=0, timeout=TIMEOUT):
        deadline = time.monotonic() + timeout
        while True:
            if needle in text(self.raw[since:]):
                return time.monotonic()
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"{needle!r} not painted; tail: {text(self.raw[-600:])!r}")
            self.pump(min(remaining, 0.05))

    def send(self, data):
        os.write(self.master, data.encode() if isinstance(data, str) else data)
        return time.monotonic()

    def type(self, data):
        for character in data:
            self.send(character)

    def close(self):
        if self.process.poll() is None:
            self.send(b"\x03")
            deadline = time.monotonic() + 10
            while self.process.poll() is None and time.monotonic() < deadline:
                try:
                    self.pump(0.05)
                except RuntimeError:
                    break
            if self.process.poll() is None:
                self.process.send_signal(signal.SIGTERM)
                self.process.wait(10)
        os.close(self.master)


def text(raw):
    return CSI.sub(b"", bytes(raw)).decode(errors="replace")


def ms(seconds):
    return round(seconds * 1000, 2)


def nearest_rank(samples, fraction):
    ordered = sorted(samples)
    rank = max(1, -(-len(ordered) * fraction // 1))
    return ordered[int(rank) - 1]


def summary(samples):
    if not samples:
        return None
    return {"n": len(samples), "median_ms": nearest_rank(samples, 0.5), "p95_ms": nearest_rank(samples, 0.95),
            "p99_ms": nearest_rank(samples, 0.99), "min_ms": min(samples), "max_ms": max(samples)}


def pinned(argv, cpu):
    if cpu is None or not shutil.which("taskset"):
        return argv
    return ["taskset", "-c", str(cpu)] + argv


def run_qq(args, home, *argv):
    result = subprocess.run(pinned([args.qq, *argv], args.qq_cpu), env=home.env, cwd=home.cwd,
                            capture_output=True, text=True, timeout=120)
    return result


def seed(args, home, count):
    sessions = {}
    for index in range(1, count + 1):
        result = run_qq(args, home, "run", "--format", "jsonl", f"mark{index:02d} seed")
        if result.returncode != 0:
            raise RuntimeError(f"seed {index} failed: {result.stdout[-400:]} {result.stderr[-400:]}")
        trial = json.loads(result.stdout.splitlines()[0])
        sessions[index] = trial["session_id"]
    return sessions


def first_frame_qa(args, scratch, port):
    samples = []
    for sample in range(args.samples):
        root = tempfile.mkdtemp(prefix="qq-tui-qa.", dir=scratch)
        os.chmod(root, 0o700)
        os.mkdir(os.path.join(root, "config"))
        with open(os.path.join(root, "config", "config.ron"), "w") as handle:
            handle.write(config_ron(port))
        env = {"PATH": os.environ["PATH"], "TERM": "xterm-256color", "HOME": root}
        tty = Tty(pinned([args.qq, "--tui-qa-root", root], args.qq_cpu), env, root)
        try:
            painted = tty.wait_for(PLACEHOLDER)
            samples.append(ms(painted - tty.started))
        finally:
            tty.close()
    return samples


def open_session(args, home, session_id, marker):
    first, transcript = [], []
    for _ in range(args.samples):
        tty = Tty(pinned([args.qq, "--session", session_id], args.qq_cpu), home.env, home.cwd)
        try:
            first.append(ms(tty.wait_for(PLACEHOLDER) - tty.started))
            transcript.append(ms(tty.wait_for(marker) - tty.started))
        finally:
            tty.close()
    return first, transcript


def switch_samples(args, home, sessions, warm, cold):
    samples = {"warm": [], "cold": []}
    for _ in range(max(1, args.samples // 4)):
        tty = Tty(pinned([args.qq, "--session", sessions[1]], args.qq_cpu), home.env, home.cwd)
        try:
            tty.wait_for("ANS01ANS")
            for kind, targets in (("warm", warm), ("cold", cold)):
                for index in targets:
                    tty.type("/sessions")
                    tty.send("\r")
                    since = tty.mark()
                    tty.type(f"mark{index:02d}")
                    tty.wait_for(f"mark{index:02d}", since)
                    since = tty.mark()
                    sent = tty.send("\r")
                    samples[kind].append(ms(tty.wait_for(f"ANS{index:02d}ANS", since) - sent))
        finally:
            tty.close()
    return samples


def keystroke_samples(args, scratch, port):
    home = Home(scratch, port, "streams")
    sessions = seed(args, home, 8)
    tty = Tty(pinned([args.qq, "--session", sessions[1]], args.qq_cpu), home.env, home.cwd)
    samples = []
    try:
        tty.wait_for("ANS01ANS")
        for stream in range(8):
            if stream:
                tty.type("/sessions")
                tty.send("\r")
                since = tty.mark()
                tty.type(f"mark{stream + 1:02d}")
                tty.wait_for(f"mark{stream + 1:02d}", since)
                since = tty.mark()
                tty.send("\r")
                tty.wait_for(f"ANS{stream + 1:02d}ANS", since)
            since = tty.mark()
            tty.type(f"STREAM {stream}")
            tty.send("\r")
            tty.wait_for(f"flow{stream}x", since)
        draft = ""
        active = []
        for _ in range(args.keystrokes):
            if len(draft) == len(DRAFT):
                since = tty.mark()
                tty.send("\x7f" * len(draft))
                tty.wait_for("Steer the run", since)
                draft = ""
            character = DRAFT[len(draft)]
            draft += character
            active.append(Fake.active[0])
            since = tty.mark()
            sent = tty.send(character)
            samples.append(ms(tty.wait_for(draft[-12:], since) - sent))
        try:
            still_streaming = bool(tty.wait_for("flow7x", tty.mark(), 2.0))
        except TimeoutError:
            still_streaming = False
    finally:
        tty.close()
    return samples, still_streaming, min(active), max(active)


def host_info():
    def read(path):
        try:
            with open(path) as handle:
                return handle.read().strip()
        except OSError:
            return None
    model = None
    cpuinfo = read("/proc/cpuinfo") or ""
    for line in cpuinfo.splitlines():
        if line.startswith("model name"):
            model = line.split(":", 1)[1].strip()
            break
    return {"uname": " ".join(platform.uname()), "nproc": os.cpu_count(), "cpu_model": model,
            "loadavg": read("/proc/loadavg"), "pressure_io": read("/proc/pressure/io"),
            "pressure_cpu": read("/proc/pressure/cpu")}


def main():
    parser = argparse.ArgumentParser(usage=__doc__)
    parser.add_argument("--qq", required=True)
    parser.add_argument("--out", required=True)
    parser.add_argument("--samples", type=int, default=20)
    parser.add_argument("--keystrokes", type=int, default=60)
    parser.add_argument("--seeds", type=int, default=12)
    parser.add_argument("--qq-cpu", type=int)
    parser.add_argument("--self-cpu", type=int)
    args = parser.parse_args()
    args.qq = os.path.abspath(args.qq)
    if args.self_cpu is not None and hasattr(os, "sched_setaffinity"):
        os.sched_setaffinity(0, {args.self_cpu})
    os.makedirs(args.out, exist_ok=True)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Fake)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    scratch = tempfile.mkdtemp(prefix="qq-b1.", dir="/tmp")
    report = {"qq": args.qq, "version": subprocess.run([args.qq, "--version"], capture_output=True,
                                                        text=True).stdout.strip(),
              "terminal": f"{COLUMNS}x{ROWS}", "samples": args.samples, "qq_cpu": args.qq_cpu,
              "self_cpu": args.self_cpu, "host_before": host_info(), "raw": {}, "notes": []}
    daemon_home = None
    try:
        report["raw"]["first_frame_qa_root"] = first_frame_qa(args, scratch, port)

        embedded = Home(scratch, port, "embedded")
        sessions = seed(args, embedded, args.seeds)
        first, transcript = open_session(args, embedded, sessions[1], "ANS01ANS")
        report["raw"]["first_frame_embedded_resume"] = first
        report["raw"]["transcript_embedded_resume"] = transcript

        newest = sorted(sessions, reverse=True)
        warm, cold = newest[:4], [index for index in newest[4:] if index != 1]
        switched = switch_samples(args, embedded, sessions, warm, cold)
        report["raw"]["switch_warm_embedded"] = switched["warm"]
        report["raw"]["switch_cold_embedded"] = switched["cold"]
        report["notes"].append(f"switch warm targets mark{warm}, cold targets mark{cold}; "
                               "/sessions, type the title, Enter; timed from Enter to the answer painted")

        daemon_home = Home(scratch, port, "daemon")
        daemon_sessions = seed(args, daemon_home, 2)
        started = run_qq(args, daemon_home, "daemon", "run", "--detach")
        report["daemon_start"] = (started.stdout + started.stderr).strip()
        if started.returncode != 0:
            raise RuntimeError(f"daemon failed: {report['daemon_start']}")
        warmup = Tty(pinned([args.qq, "--session", daemon_sessions[1]], args.qq_cpu), daemon_home.env,
                     daemon_home.cwd)
        warmup.wait_for("ANS01ANS")
        warmup.close()
        first, transcript = open_session(args, daemon_home, daemon_sessions[1], "ANS01ANS")
        report["raw"]["first_frame_daemon_resume"] = first
        report["raw"]["transcript_daemon_resume"] = transcript

        keys, streaming, fewest, most = keystroke_samples(args, scratch, port)
        report["raw"]["keystroke_echo_8_streams"] = keys
        report["notes"].append("keystroke echo: 8 seeded sessions, each opened via /sessions and sent a "
                               f"streaming prompt; provider streams open per sample min {fewest} max {most}, "
                               f"a delta every {Fake.delta_interval * 1000:.0f} ms each; "
                               f"focused still streaming after: {streaming}")
    finally:
        if daemon_home is not None:
            stopped = run_qq(args, daemon_home, "daemon", "stop")
            report["daemon_stop"] = (stopped.stdout + stopped.stderr).strip()
        server.shutdown()
        report["host_after"] = host_info()
        report["summary"] = {name: summary(values) for name, values in report["raw"].items()}
        with open(os.path.join(args.out, "recorder.json"), "w") as handle:
            json.dump(report, handle, indent=2)
        shutil.rmtree(scratch, ignore_errors=True)
    for name, values in report["summary"].items():
        print(name, json.dumps(values))


if __name__ == "__main__":
    sys.exit(main())
