#!/usr/bin/env bash
# Serves `dist/` for the spike pages. The qq server must allow this origin:
#   qq serve --allow-origin http://127.0.0.1:8090
set -euo pipefail
cd "$(dirname "$0")/dist"
exec python3 -m http.server "${1:-8090}" --bind 127.0.0.1
