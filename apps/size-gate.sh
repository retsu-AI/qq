#!/usr/bin/env bash
# Fails when any artifact (one wasm plus its JS glue, gzip-compressed) exceeds
# the ADR-0017 budget. Run after build.sh. Override with QQ_UI_SIZE_BUDGET.
set -euo pipefail
cd "$(dirname "$0")"

budget=${QQ_UI_SIZE_BUDGET:-600000}
status=0

gzip_bytes() {
  gzip -9 -c "$1" | wc -c | tr -d ' '
}

check() {
  local label=$1 dir=$2
  local wasm glue wasm_gz glue_gz total
  wasm=$(find "$dir" -maxdepth 1 -name '*_bg.wasm' | head -n 1)
  if [ -z "$wasm" ]; then
    echo "size-gate: $label: no wasm in $dir" >&2
    return 1
  fi
  glue="${wasm%_bg.wasm}.js"
  wasm_gz=$(gzip_bytes "$wasm")
  glue_gz=$(gzip_bytes "$glue")
  total=$((wasm_gz + glue_gz))
  local verdict="ok"
  if [ "$total" -gt "$budget" ]; then
    verdict="OVER BUDGET"
    status=1
  fi
  printf '%-10s wasm %8d  glue %6d  total %8d / %d gzip bytes  %s\n' "$label" "$wasm_gz" "$glue_gz" "$total" "$budget" "$verdict"
}

check shell dist
for remote in dist/remotes/*/; do
  check "$(basename "$remote")" "$remote"
done
exit $status
