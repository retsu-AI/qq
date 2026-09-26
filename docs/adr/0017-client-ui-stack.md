# ADR-0017 — Client UI stack: Leptos CSR remotes behind a framework-neutral ES-module contract

**Status:** Accepted (founder decision 2026-09-24: Leptos; evidence and recommendation below)
**Date:** 2026-09-24
**Deciders:** founder; multi-surface lead
**Implements:** `docs/plans/multi-surface-clients.md` U1 (framework), U2–U7 (Sessions remote), D1–D3 / M1–M3 (Tauri v2 shells)

## Context

The multi-surface plan fixed the shape of the web client before choosing how
it renders: a Rust/WASM SPA hosted separately from `qq serve`, talking to many
servers directly over HTTP JSON + SSE, sharing `qq-client::state` with the
TUI, wrapped in Tauri v2 for desktop and mobile. What was left open was the
UI framework — Leptos or Dioxus — to be settled by a spike with measured exit
criteria rather than by preference.

The web surface is also a micro-frontend: one shell that hosts independently
deployed remotes (Sessions first; Fleet, Approvals/Audit, Billing, Onboarding
later, owned by the layer above QQ per ADR-0009). Whatever is chosen must
leave the shell ↔ remote boundary usable by a remote built with a different
stack, or the boundary is not a boundary.

The spike lives in `benchmarks/wasm-ui-spike/` and compares three candidates
that share one `spike-common` crate — the authenticated `/v1/health` probe,
`ServerConnection`, `SessionClient`, workspace resolution, initial snapshot,
cursor-addressed `fetch` SSE with reconnect, and the `SessionStore` reducer —
so the candidates differ only in rendering and runtime integration:

- `baseline`: no framework, `innerHTML` render. Measures what the client
  itself costs so framework overhead can be isolated.
- `leptos`: Leptos 0.8.20, client-side rendering, `mount_to` / `UnmountHandle`.
- `dioxus`: Dioxus 0.7.10, `dioxus-web`, `VirtualDom` + `dioxus::web::run`.

Each candidate is built by Trunk 0.21 with the same release profile
(`opt-level = "z"`, fat LTO, one codegen unit, `panic = "abort"`) and
Trunk-managed `wasm-opt` (Binaryen `version_123`; the system Binaryen 105
cannot parse the reference-types output Dioxus needs). Live traffic came from
a real `qq serve --allow-origin` on loopback, driven by a deterministic
OpenAI-compatible fake model (`fake_model.py`) streaming thousands of deltas
per run; measurements were taken in Chrome over CDP (`run_browser.py`).

## Decision

1. **Framework: Leptos 0.8 (client-side rendering) for the shell and the
   Sessions remote.** Recommendation; see the evidence. The founder may
   override to Dioxus without changing anything below except the framework
   crates of `apps/shell` and `apps/sessions`.

2. **Remote contract is framework-neutral and lives at the ES-module
   boundary, not inside either framework.** A remote is a `wasm-bindgen`
   output (`<name>.js` + `<name>_bg.wasm`) served from its own origin/path and
   loaded by the shell with dynamic `import()`. It exports exactly:

   ```text
   default(init)                          // wasm-bindgen initializer
   mount(root: HtmlElement, config: string) -> Result<(), JsValue>
   unmount() -> ()
   ```

   `config` is a JSON document owned by `qq-client` types (server set,
   credential references, focused workspace). Nothing crosses the boundary
   as a framework value: no shared signals, no shared virtual DOM, no shared
   `wasm-bindgen` instance. This is what "module-federation-style" means for
   Rust/WASM — there is no shared module graph to federate, so the contract
   is the loader plus two functions. Fleet/Approvals/Billing/Onboarding
   remotes implement this contract and nothing else; they may use any stack.

3. **Each wasm instance owns its own client; state is shared by protocol,
   not by memory.** Separate wasm modules cannot share a `SessionClient` or
   a `SessionStore` without serializing every event across the JS boundary,
   which would re-implement the wire protocol in-page. So: the shell owns
   server profiles, credentials, and the W3 `ServerSet` connection loops for
   the Overview; a remote receives in `config` which servers/workspaces to
   bind and opens its own `SessionClient` + SSE subscription against `qq
   serve`, reducing with `qq-client::state` inside its own instance. The
   spike host does exactly this (two remotes, two clients, two subscriptions,
   one page) and the per-remote cost is one extra SSE connection and ~350 KB
   gzip of shared-by-source, not shared-by-link, client code. The reducer
   itself is never duplicated in source — every remote compiles
   `qq-client::state`.

4. **Tauri v2 shells embed the same static Trunk output** (`frontendDist`)
   for desktop and mobile. Remotes load over the network in the browser and
   from the bundled `dist/` in Tauri; the contract does not change.

5. **Size budget for U1's gate: 600 KB gzipped per artifact (decimal
   kilobytes; all sizes in this ADR are decimal), measured on wasm + JS glue
   of the shell and of the Sessions remote separately**, refined from the
   plan's placeholder 1.5 MiB. The measured floor (client + reducer + serde
   + reqwest) is 348 KB gzip; Leptos adds 31 KB. That leaves ~220 KB per
   artifact for U2–U5 UI, `pulldown-cmark`, and diffs. Because of (3) a
   first page load fetches shell + Sessions ≈ 2 × the floor; both are
   immutable, content-hashed, and cached by the PWA service worker, so the
   gate is per artifact, not per page. The gate is a CI assertion that fails
   the build when exceeded.

## Consequences

- One framework in `apps/` for QQ-owned surfaces (shell, Sessions, the
  Tauri wrappers). Other remotes are free to differ because the contract is
  the ES module, but a remote in another stack pays its own runtime (a
  Dioxus remote next to a Leptos shell ships two runtimes; measured below).
- Trunk is the build tool; `dx` is not required. Tauri's official Leptos
  guide is Trunk-based, so D1/M1 follow documented paths.
- Leptos's `!Send` reactive values (`RwSignal::new_local`,
  `StoredValue::new_local`) match `SessionClient` and the SSE stream, which
  are `!Send` on `wasm32`; no `Send` shims are needed. The `ClientPort: Send`
  bound noted open in W2 is still a W3 item.
- Unmount is a supported Leptos API (`UnmountHandle::unmount`). Dioxus has no
  unmount for a mounted `VirtualDom`; the spike aborts the renderer future
  and clears the root, which works but is not a documented lifecycle.
  Independently deployable remotes need unmount to be part of the contract,
  which weighed against Dioxus for the *shell*.
- Server-side rendering and hydration are out of scope; `qq serve` does not
  serve the UI and the hosted app is a static SPA/PWA.
- If Leptos stalls (0.8 → 0.9 churn, or wasm mobile issues surface in M1),
  the fallback order is Dioxus (same contract, measured), then the plan's TS
  SPA fallback (same contract over `qq-client` compiled to wasm).

## Alternatives considered

**Dioxus 0.7 for everything.** Viable — it passed every functional check in
the spike (live SSE at 300–600 events/s, remote mount into a host slot,
Tauri-compatible static build). Rejected for the shell on three measured or
documented points: +83 KB gzip over Leptos for the same UI, 2.3× the JS glue
(86 KB vs 37 KB raw), and no supported unmount. Its strengths (hot
patching, `dx` tooling, first-party desktop/mobile renderers via Wry) are
either build-time conveniences or compete with Tauri rather than compose with
it; the plan already commits to Tauri v2 for the shells.

**Stack-agnostic shell with no framework.** The `baseline` candidate shows a
plain-DOM shell is 31 KB gzip smaller than Leptos, but the shell has real
UI (servers screen, overview, workspace/session tree) and hand-rolled DOM
diffing would be re-implementing a framework badly. The remote *contract* is
stack-agnostic; the shell implementation is not required to be.

**Pick per remote (Leptos shell, Dioxus Sessions).** Measured cost is two
runtimes per page load (~+115 KB gzip) for no functional gain while both
are QQ-owned. Kept as an *option* the contract permits for third-party
remotes.

**TypeScript SPA.** The plan's documented fallback. Not spiked: W1/W2
already made `qq-client` compile for `wasm32`, so the reducer and protocol
would either be duplicated in TS or wrapped in wasm anyway; the Rust
candidates measured well within budget.

## Evidence / references

Spike: `benchmarks/wasm-ui-spike/` (`README.md` there has the reproduction
steps). Numbers below are from one Linux x86-64 host, Rust 1.97.1, Trunk
0.21.14, Chrome via CDP, 2026-09-24.

### Bundle size (release, `wasm-opt -Oz`, reference types + weak refs)

| Candidate | wasm raw | wasm gzip | wasm brotli | JS glue raw | JS glue gzip |
| --- | ---: | ---: | ---: | ---: | ---: |
| baseline (client + reducer, `innerHTML`) | 990,623 | 347,845 | 271,281 | 33,684 | 6,845 |
| Leptos 0.8.20 CSR | 1,063,916 | 378,557 | 294,541 | 37,363 | 7,453 |
| Dioxus 0.7.10 web | 1,253,145 | 461,471 | 357,886 | 85,682 | 13,400 |

Framework overhead over the baseline: Leptos +30.7 KB gzip wasm, +0.6 KB
glue; Dioxus +113.6 KB gzip wasm, +6.6 KB glue. Both are far under the
plan's 1.5 MiB placeholder; the client itself (`reqwest` + `serde_json` +
protocol types + reducer) is the dominant cost and is framework-independent.

### SSE streaming ergonomics

Both candidates consume the same `spike_common::run_feed(client, config,
sink)` loop; the sink is a closure that writes into one reactive value
(`RwSignal<Feed>` / `Signal<Feed>`). No framework-specific transport code was
needed. Per-event reducer cost (`apply ms`) and frame health
(`long frames` = frames > 50 ms, measured with `requestAnimationFrame`)
during live streaming, transcript pane open on a streaming session:

| Run | Candidate | events | ev/s | apply mean/max ms | long frames / max frame | DOM nodes |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 3 sessions, 300 deltas/s | baseline | 2,938 | 289 | 0.003 / 0.2 | 0 / 20 ms | 73 |
| | Leptos | 2,986 | 295 | 0.004 / 0.1 | 0 / 25 ms | 86 |
| | Dioxus | 2,966 | 292 | 0.003 / 0.2 | 0 / 23 ms | 95 |
| 6 sessions, 2000 deltas/s, run A | baseline | 3,104 | 556 | 0.005 / 1.0 | 2 / 184 ms | 163 |
| | Leptos | 2,380 | 487 | 0.003 / 0.1 | 3 / 234 ms | 185 |
| | Dioxus | 2,555 | 593 | 0.004 / 0.1 | 0 / 29 ms | 203 |
| 6 sessions, 2000 deltas/s, run B | baseline | 2,482 | 574 | 0.004 / 0.3 | 0 / 47 ms | — |
| | Leptos | 2,521 | 581 | 0.004 / 0.2 | 0 / 41 ms | — |
| | Dioxus | 2,518 | 585 | 0.003 / 0.1 | 1 / 61 ms | — |

Reading: rendering cost is indistinguishable between the two frameworks at
these rates; the sporadic long frames in the stress runs land on different
candidates in consecutive runs and coincide with snapshot install and
run-start bursts, not with steady streaming. Streaming ergonomics do not
decide the ADR. (Chrome JS heap after the stress run: baseline 3.9 MiB,
Leptos 4.7 MiB, Dioxus 20.1 MiB — one sample, not controlled for GC timing;
recorded, not weighed.)

### Remote loading (module-federation-style)

`host/index.html` is a plain ES-module page with no framework. It
`import()`s each candidate's `wasm-bindgen` JS, runs the initializer, and
calls `mount(slot, configJson)`, then `unmount()`:

| Remote | `import()` | init (wasm instantiate) | `mount()` | unmount leaves nodes |
| --- | ---: | ---: | ---: | ---: |
| Leptos | 4–5 ms | 7–8 ms | 0.9–3.0 ms | 0 |
| Dioxus | 10–12 ms | 9–10 ms | 0.3–0.4 ms | 0 |

Both remotes mount side by side in the same page, each with its own wasm
instance, client, SSE connection, and reducer. Leptos unmount is
`UnmountHandle::unmount()`; Dioxus unmount is
`AbortHandle::abort()` on the `dioxus::web::run` future plus clearing the root
— functional, but outside the documented API.

### Tauri v2 fit

- Tauri's frontend guide lists Leptos among the officially maintained
  `create-tauri-app` templates and has a dedicated Leptos page: Trunk
  `serve`/`build` as `beforeDevCommand`/`beforeBuildCommand`, `frontendDist`
  pointing at Trunk's `dist/`, `withGlobalTauri: true`, and
  `serve.ws_protocol = "ws"` for mobile hot reload. This is the exact build
  the spike uses.
- Dioxus web is also a static Trunk build and would work as `frontendDist`
  the same way (inference from the spike build; not an officially documented
  Tauri path — Dioxus is absent from Tauri's template list). Dioxus's own
  native story is `dioxus-desktop`/`dioxus-mobile` on Wry, which is a
  parallel shell to Tauri, not an integration with it.
- Neither framework changes how a remote is loaded inside a Tauri webview:
  the shell imports the same ES module from the bundled `dist/`.

### Screenshots

Taken by `run_browser.py` during the 3-session run; the PR that introduces
this ADR embeds them. Standalone Leptos and Dioxus pages show the same
grouped session list (NEEDS YOU / WORKING / IDLE / DONE), live status bar, and
a focused streaming transcript; the host page shows both remotes mounted in
separate slots and the empty slots after unmount.

### References

- `docs/plans/multi-surface-clients.md` — U1 size gate, U2–U7, D/M phases.
- ADR-0009 — hosting boundary; remotes other than Sessions are owned above QQ.
- `docs/design/protocol.md` — why browser SSE is `fetch`-streamed, not
  `EventSource`.
- Tauri v2 docs: "Frontend Configuration" and "Leptos" guide (fetched
  2026-09-24).
- Leptos 0.8 `leptos::mount::mount_to` / `UnmountHandle`; Dioxus 0.7
  `dioxus::web::run`, `dioxus::web::Config::rootelement` (crate sources as
  pinned in `benchmarks/wasm-ui-spike/Cargo.lock`).
