# Isolated TUI QA fixture

Use this profile to inspect the real QQ TUI without reading the user's QQ
configuration, credential index, session database, or server-discovery file.
It is a local diagnostic fixture, not an Astra, JEV, authentication, or
production acceptance test.

## Prepare the fixture

Choose a new private directory and a free loopback port. Before QQ starts, the
root must contain exactly a real `config` directory with one regular, non-linked
`config.ron`; QQ refuses pre-existing data, credential, runtime, managed, or
workspace paths. It then creates those five state directories itself. The only
fixture input is the local provider configuration:

```sh
QA_ROOT="$(mktemp -d /tmp/qq-tui-qa.XXXXXX)"
chmod 700 "$QA_ROOT"
mkdir -p "$QA_ROOT/config"
cat >"$QA_ROOT/config/config.ron" <<'RON'
(
  version: 1,
  model: "custom/qa-model",
  providers: {
    "custom": Custom(
      connection: (
        base_url: "http://127.0.0.1:18081/v1",
        api: OpenAiResponses,
        auth: NoAuth,
      ),
      models: { "qa-model": (name: "Local QA model") },
    ),
  },
)
RON
```

Run this credential-free fake Responses endpoint in a separate terminal. It
binds only to loopback and returns one deterministic response:

```sh
python3 - <<'PY'
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BODY = (
    'data: {"type":"response.output_text.delta","delta":"Local TUI QA response."}\n\n'
    'data: {"type":"response.completed","response":{"usage":'
    '{"input_tokens":1,"output_tokens":5}}}\n\n'
).encode()

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/v1/responses":
            self.send_error(404)
            return
        length = int(self.headers.get("content-length", "0"))
        self.rfile.read(length)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(BODY)))
        self.end_headers()
        self.wfile.write(BODY)
    def log_message(self, *_args):
        pass

ThreadingHTTPServer(("127.0.0.1", 18081), Handler).serve_forever()
PY
```

To review transcript rendering end to end, run this endpoint instead. It
streams the markdown gallery (the same text `qq_tui::bench_support::
MARKDOWN_GALLERY` pins in `crates/qq-tui/tests/goldens/`) as one delta per
line with a short pause, so streaming layout, the settled-prefix cache, and
off-tick highlighting are all exercised in a real terminal:

```sh
python3 - <<'PY'
import json, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

GALLERY = """First paragraph of prose.

Second paragraph of prose, directly after the first.

# Level one heading

## Level two heading

### Level three heading

1. First numbered item
2. Second numbered item that is deliberately long enough to wrap onto a second row at this width
3. Third numbered item
   - nested bullet under three

- A bullet item that is also deliberately long enough to wrap onto a second physical row here
- Short bullet

- [ ] open task
- [x] done task

> A quote long enough to wrap onto a second row so we can see whether the rail repeats.

Some *emphasis*, some **strong**, some `inline code`, a [link](https://example.com/x), a footnote[^1], and math $x^2$.

[^1]: The footnote body.

```rust
fn main() {
    let x = 1;
    if x > 0 {
        println!("{x}");
    }
}
```

| Role | Default |
| --- | --- |
| text | white |
| muted | dark grey |

---

Tail paragraph.
"""

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/v1/responses":
            self.send_error(404)
            return
        length = int(self.headers.get("content-length", "0"))
        self.rfile.read(length)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.end_headers()
        for line in GALLERY.splitlines(keepends=True):
            event = {"type": "response.output_text.delta", "delta": line}
            self.wfile.write(f"data: {json.dumps(event)}\n\n".encode())
            self.wfile.flush()
            time.sleep(0.04)
        done = {"type": "response.completed",
                "response": {"usage": {"input_tokens": 1, "output_tokens": 200}}}
        self.wfile.write(f"data: {json.dumps(done)}\n\n".encode())
        self.wfile.flush()
    def log_message(self, *_args):
        pass

ThreadingHTTPServer(("127.0.0.1", 18081), Handler).serve_forever()
PY
```

Ask anything; the reply is always the gallery. Check it at a full-screen
window and again at 80 × 24 (`resize` or a split), and with `/theme` to
switch palettes. The same frames without a terminal are written by
`cargo test -p qq-tui --test gallery -- --ignored` to
`target/qq-tui-gallery/<theme>/<scene>-<w>x<h>.ans` for `cat`.

From a real terminal, with no enforced-review environment override, start the
fixture:

```sh
qq --tui-qa-root "$QA_ROOT"
```

The profile fails before runtime startup if `QQ_JEV_CHECKPOINTS=enforce` is
present, because a credential-free fixture cannot satisfy mandatory review.
It ignores unselected providers and rejects a remote or authenticated selected
provider, static headers, stored credentials, organizations, MCP, worker or
reviewer routes, delegation, audit, profiles, and packs. The isolation policy
remains attached to the runtime factory across model catalogs, session loads,
reconnects, spawn checks, grant reads, and capability refreshes; none of those
callbacks reconstructs QA configuration from the process environment. It never
changes or unsets those settings. Fixture directories and files are rechecked
throughout that lifecycle, and QQ fails closed if a path is replaced by a
symbolic link or a regular file is hard-linked outside the fixture. Bare `qq`,
`qq run`, and every other command retain their production behavior; the option
is valid only for bare interactive QQ.

## Expected isolated paths

| Purpose | Path |
| --- | --- |
| global configuration | `$QA_ROOT/config` |
| trust and session database | `$QA_ROOT/data` |
| empty credential index | `$QA_ROOT/credentials` |
| managed file fixture | `$QA_ROOT/managed` |
| server lock and discovery | `$QA_ROOT/runtime` |
| TUI workspace and project config | `$QA_ROOT/workspace` |

Stop QQ and the fake endpoint with `Ctrl-C`. After confirming both processes
have exited, remove only the exact temporary fixture printed in `QA_ROOT`:

```sh
case "$QA_ROOT" in
  /tmp/qq-tui-qa.*) rm -rf -- "$QA_ROOT" ;;
  *) echo "refusing to remove unexpected QA_ROOT: $QA_ROOT" >&2 ;;
esac
```

Do not use a user configuration, data, credential, or runtime directory as
`QA_ROOT`. Each launch requires a new root; preserve an old fixture instead of
reopening it when its SQLite state or logs are needed for a defect report.
