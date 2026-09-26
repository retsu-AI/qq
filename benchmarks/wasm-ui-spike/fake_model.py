#!/usr/bin/env python3
"""A deterministic OpenAI Chat Completions endpoint that streams `--deltas`
small text chunks at `--rate` chunks per second. It gives the spikes a
credential-free, repeatable event stream through a real `qq serve`.

    python3 fake_model.py --port 9080 --deltas 2000 --rate 200

Point qq at it with a `Custom` provider:

    providers: { "fake": Custom(connection: (base_url: "http://127.0.0.1:9080/v1",
        api: OpenAiChatCompletions, auth: NoAuth), models: { "stream": (name: "Fake") }) }
"""

import argparse
import json
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

WORDS = ("alpha ", "beta ", "gamma ", "delta ", "epsilon ", "zeta ", "eta ", "theta ")


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    deltas = 500
    rate = 100.0

    def log_message(self, *_args):
        return

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        self.rfile.read(length)
        if not self.path.endswith("/chat/completions"):
            self.send_response(404)
            self.send_header("content-length", "0")
            self.end_headers()
            return
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("transfer-encoding", "chunked")
        self.end_headers()
        interval = 1.0 / self.rate if self.rate > 0 else 0.0
        started = time.monotonic()
        for index in range(self.deltas):
            text = WORDS[index % len(WORDS)]
            if index % 16 == 15:
                text += "\n"
            self._chunk(
                {
                    "id": "spike",
                    "object": "chat.completion.chunk",
                    "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": None}],
                }
            )
            deadline = started + (index + 1) * interval
            now = time.monotonic()
            if deadline > now:
                time.sleep(deadline - now)
        self._chunk({"id": "spike", "object": "chat.completion.chunk", "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]})
        self._chunk({"id": "spike", "object": "chat.completion.chunk", "choices": [], "usage": {"prompt_tokens": 12, "completion_tokens": self.deltas}})
        self._raw(b"data: [DONE]\n\n")
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()

    def _chunk(self, payload):
        self._raw(b"data: " + json.dumps(payload).encode() + b"\n\n")

    def _raw(self, body):
        self.wfile.write(f"{len(body):x}\r\n".encode() + body + b"\r\n")
        self.wfile.flush()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--port", type=int, default=9080)
    parser.add_argument("--deltas", type=int, default=500)
    parser.add_argument("--rate", type=float, default=100.0)
    args = parser.parse_args()
    Handler.deltas = args.deltas
    Handler.rate = args.rate
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"fake model streaming {args.deltas} deltas at {args.rate}/s on 127.0.0.1:{args.port}", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
