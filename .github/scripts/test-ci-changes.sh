#!/usr/bin/env bash
set -euo pipefail
source_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
git_local_env=$(git rev-parse --local-env-vars)
for git_env_name in $git_local_env; do unset "$git_env_name"; done
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
fixture=$(mktemp -d "${TMPDIR:-/tmp}/qq-ci-changes.XXXXXX")
trap 'rm -rf -- "$fixture"' EXIT
cd "$fixture"
git init --quiet --template= -b main .
commit() {
  git add -A
  git -c user.name=Fixture -c user.email=fixture@example.invalid -c commit.gpgsign=false \
    commit --quiet --allow-empty -m "$1"
}
mkdir -p crates/qq-core/src/sessions crates/qq-core/src/tools crates/qq-tui/src crates/qq-client/src docs
printf 'fn a() {}\n' >crates/qq-core/src/sessions/runs.rs
printf 'fn a() {}\n' >crates/qq-tui/src/view.rs
commit base

expect() {
  local label=$1 want=$2 got
  commit "$label"
  got=$(bash "$source_root/.github/scripts/ci-changes.sh" main | tr '\n' ' ')
  git reset --quiet --hard main
  if [[ $got != "$want " ]]; then
    echo "$label: want '$want', got '$got'" >&2
    exit 1
  fi
}

none='core=false wasm=false old_git=false devcontainer=false platform=false'
git checkout --quiet -b change

printf 'more\n' >docs/guide.md
expect 'docs only' "$none"

printf 'fn b() {}\n' >>crates/qq-tui/src/view.rs
expect 'a portable tui change' "$none"

printf 'fn b() {}\n' >>crates/qq-core/src/sessions/runs.rs
expect 'a portable core change' 'core=true wasm=false old_git=true devcontainer=false platform=false'

printf '#[cfg(windows)]\nfn c() {}\n' >>crates/qq-tui/src/view.rs
expect 'a file with platform cfg' 'core=false wasm=false old_git=false devcontainer=false platform=true'

printf 'fn d() {}\n' >crates/qq-core/src/tools/shell.rs
expect 'the shell tool' 'core=true wasm=false old_git=true devcontainer=false platform=true'

printf 'fn e() {}\n' >crates/qq-client/src/lib.rs
expect 'the client' 'core=false wasm=true old_git=false devcontainer=false platform=false'

printf '[workspace]\n' >Cargo.toml
expect 'the root manifest' 'core=true wasm=true old_git=true devcontainer=true platform=true'

[[ $(bash "$source_root/.github/scripts/ci-changes.sh" all | tr '\n' ' ') == 'core=true wasm=true platform=true old_git=true devcontainer=true ' ]]
echo "ci-changes fixtures: ok"
