#!/usr/bin/env bash
# Builds both spikes with identical release settings and reports the bundle
# sizes the ADR cites. Output layout (served by `serve.sh`):
#   dist/index.html        remote-loading host
#   dist/leptos/…          Leptos standalone page + remote module
#   dist/dioxus/…          Dioxus standalone page + remote module
set -euo pipefail
cd "$(dirname "$0")"

rm -rf dist
mkdir -p dist
cp host/index.html spike.css dist/

for fw in baseline leptos dioxus; do
  (cd "$fw" && trunk build --release --filehash false --public-url "/$fw/" --dist "../dist/$fw" index.html)
done

size() { stat -c %s "$1"; }
gz() { gzip -9 -c "$1" | wc -c; }
br() { if command -v brotli >/dev/null; then brotli -q 11 -c "$1" | wc -c; else echo "-"; fi; }

echo
echo "| candidate | wasm raw | wasm gzip | wasm brotli | js glue raw | js glue gzip |"
echo "| --- | ---: | ---: | ---: | ---: | ---: |"
for fw in baseline leptos dioxus; do
  wasm="dist/$fw/spike-${fw}_bg.wasm"
  js="dist/$fw/spike-${fw}.js"
  echo "| $fw | $(size "$wasm") | $(gz "$wasm") | $(br "$wasm") | $(size "$js") | $(gz "$js") |"
done
echo
echo "toolchain: $(rustc --version); $(trunk --version); wasm-opt $(grep wasm_opt leptos/Trunk.toml | cut -d'"' -f2) (trunk-managed, -Oz)"
echo "leptos $(grep -A1 'name = "leptos"' Cargo.lock | tail -1)"
echo "dioxus $(grep -A1 'name = "dioxus"' Cargo.lock | tail -1)"
