# ADR-0017 spike — Leptos vs Dioxus on `qq-client`

Evidence for [`docs/adr/0017-client-ui-stack.md`](../../docs/adr/0017-client-ui-stack.md).
Three `wasm32` candidates share one `spike-common` crate (authenticated
health probe, `ServerConnection`, `SessionClient`, snapshot, `fetch` SSE with
reconnect, `SessionStore` reducer, frame monitor) so they differ only in
rendering:

| Crate | Renders with | Mount / unmount |
| --- | --- | --- |
| `baseline` | none (`innerHTML`) | own |
| `leptos` | Leptos 0.8 CSR | `mount_to` / `UnmountHandle` |
| `dioxus` | Dioxus 0.7 web | `dioxus::web::run` under an `AbortHandle` |

Each candidate exports the remote contract the ADR fixes:
`mount(root, configJson)` and `unmount()`. `host/index.html` is a plain
ES-module page that loads both remotes into separate slots without knowing
their framework.

This is a separate Cargo workspace. It is not part of the root gates and
`apps/` does not depend on it.

## Tools

`rustup target add wasm32-unknown-unknown` (already in `rust-toolchain.toml`),
`cargo install trunk --locked` (0.21). Trunk downloads the pinned
`wasm-bindgen` and `wasm-opt` from `*/Trunk.toml`. Python 3 with `playwright`
for the browser run (`pip install playwright`; it attaches to a running Chrome
over CDP, it does not launch one).

## Reproduce

```sh
# 1. bundle sizes → dist/<candidate>/ and a table on stdout
./measure.sh

# 2. a deterministic streaming provider on loopback
python3 fake_model.py --port 9080 --deltas 2000 --rate 300 &

# 3. a qq server whose `fake/stream` model routes to it
QQ_CONFIG_CONTENT='(version: 1, model: "fake/stream", providers: { "fake": Custom(
  connection: (base_url: "http://127.0.0.1:9080/v1", api: OpenAiChatCompletions, auth: NoAuth),
  models: { "stream": (name: "Fake stream", context_window: 128000, max_output_tokens: 32000) }) })' \
  qq serve --bind 127.0.0.1:9077 --allow-origin http://127.0.0.1:8090 &
./serve.sh 8090 &            # static server for dist/

# 4. drive sessions and measure in Chrome (CDP at http://localhost:29229)
python3 run_browser.py --workspace /path/to/workspace --sessions 3
```

`run_browser.py` opens each standalone candidate with the local server URL,
token, and workspace in the query string (spike only — production credentials
live in IndexedDB, never in URLs), waits for `conn live`, submits prompts via
`drive.sh`, records the status bar, DOM node count, JS heap, and a screenshot
under `dist/shots/`, then loads `host/` and checks that both remotes mount and
unmount cleanly.

`dist/` and `target/` are generated and ignored.
