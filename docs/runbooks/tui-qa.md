# Isolated TUI QA fixture

Use this profile to inspect the real QQ TUI without reading the user's QQ
configuration, credential index, session database, or server-discovery file.
It is a local diagnostic fixture, not an Astra, JEV, authentication, or
production acceptance test.

## Prepare the fixture

Choose an empty directory and a free loopback port. The application creates
the six child directories itself; the only fixture input is the local provider
configuration:

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

From a real terminal, with no enforced-review environment override, start the
fixture:

```sh
qq --tui-qa-root "$QA_ROOT"
```

The profile fails before runtime startup if `QQ_JEV_CHECKPOINTS=enforce` is
present, because a credential-free fixture cannot satisfy mandatory review.
It also rejects remote endpoints, authentication/static headers, MCP, worker
or reviewer routes, delegation, audit, profiles, and packs. It never changes
or unsets those settings. Bare `qq`, `qq run`, and every other command retain
their production behavior; the option is valid only for bare interactive QQ.

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
`QA_ROOT`. Preserve the fixture instead of removing it when its SQLite state
or logs are needed for a defect report.
