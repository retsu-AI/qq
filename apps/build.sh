#!/usr/bin/env bash
# Builds every surface into apps/dist/ in the layout the shell expects:
#
#   dist/                      the shell (hashed assets, remotes.json, sw.js)
#   dist/remotes/<remote>/     each remote, unhashed so remotes.json is stable
#
# Remotes are independently deployable: any origin may serve
# dist/remotes/<remote>/ and remotes.json may point at it.
set -euo pipefail
cd "$(dirname "$0")"

profile=${1:-release}
flag=""
if [ "$profile" = "release" ]; then
  flag="--release"
fi

rm -rf dist
(cd shell && trunk build $flag --dist ../dist index.html)
for remote in sessions; do
  (cd "$remote" && trunk build $flag --filehash false --public-url "/remotes/$remote/" --dist "../dist/remotes/$remote" index.html)
done
