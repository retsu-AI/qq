#!/bin/sh
set -eu

canonical=${1:-/Users/romanmondello/plugins/jev-codex-plugin-cross-host/contracts/jev}
root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

for name in contract.json conformance.json; do
    cmp -s "$canonical/$name" "$root/contracts/jev/$name" || {
        echo "Jev contract drift detected in $name" >&2
        exit 1
    }
done
echo "Jev contract copy matches $canonical"
