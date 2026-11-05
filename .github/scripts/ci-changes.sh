#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo 'usage: ci-changes.sh all | <base-ref>' >&2
  exit 2
}
[[ $# -eq 1 ]] || usage

flags=(core wasm platform old_git devcontainer)
if [[ $1 == all ]]; then
  for flag in "${flags[@]}"; do
    echo "$flag=true"
  done
  exit 0
fi

merge_base=$(git merge-base "$1" HEAD)
mapfile -t changed < <(git diff --name-only --no-renames "$merge_base" HEAD)

shared=(
  Cargo.toml Cargo.lock rust-toolchain.toml '.cargo/*' '.config/*'
  .github/workflows/ci.yml .github/scripts/ci-changes.sh
)
core=('crates/qq-core/*' 'crates/qq-provider/*' 'crates/qq-protocol/*' 'crates/qq-reasoning/*')
wasm=('crates/qq-client/*' 'crates/qq-protocol/*' 'crates/qq-reasoning/*')
platform=(
  'crates/qq-core/src/tools/*' crates/qq-core/src/tools.rs
  'crates/qq-core/src/workspace/*' crates/qq-core/src/workspace.rs
  crates/qq-core/src/undo.rs 'crates/qq-auth/*' 'crates/qq-config/*' 'crates/qq-server/*'
  'src/daemon*' 'tests/*' xtask/src/smoke.rs .github/scripts/platform-tests.sh
)
old_git=('crates/qq-core/*' xtask/src/smoke.rs)
devcontainer=('.devcontainer/*' 'Dockerfile*' rust-toolchain.toml Cargo.lock)

touches() {
  local -n inputs=$1
  local path input
  for path in "${changed[@]}"; do
    for input in "${inputs[@]}" "${shared[@]}"; do
      [[ $path == $input ]] && return 0
    done
  done
  return 1
}

platform_cfg() {
  local path rust=()
  for path in "${changed[@]}"; do
    [[ $path == *.rs && -f $path ]] && rust+=("$path")
  done
  [[ ${#rust[@]} -gt 0 ]] &&
    git grep -q -E 'cfg\((all\(|any\(|not\()*(windows|unix|target_os|target_family)' -- "${rust[@]}"
}

for flag in core wasm old_git devcontainer; do
  if touches "$flag"; then echo "$flag=true"; else echo "$flag=false"; fi
done
if touches platform || platform_cfg; then echo platform=true; else echo platform=false; fi
