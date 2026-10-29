#!/usr/bin/env bash
set -euo pipefail
source_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
git_local_env=$(git rev-parse --local-env-vars)
for git_env_name in $git_local_env; do unset "$git_env_name"; done
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null
hook_fixture_root=$(mktemp -d "${TMPDIR:-/tmp}/qq-pre-push.XXXXXX")
trap 'rm -rf -- "$hook_fixture_root"' EXIT
mkdir "$hook_fixture_root/bin"
cat > "$hook_fixture_root/bin/cargo" <<'CARGO'
#!/usr/bin/env sh
set -eu
[ "$*" = 'xtask check --profile quick' ]
git init --quiet --template= "$QQ_HOOK_FIXTURE"
cd "$QQ_HOOK_FIXTURE"
printf 'nested fixture\n' > nested.txt
git add nested.txt
git -c user.name=Fixture -c user.email=fixture@example.invalid -c commit.gpgsign=false commit --quiet -m nested
printf 'validated\n' > "$QQ_HOOK_MARKER"
CARGO
chmod +x "$hook_fixture_root/bin/cargo"
for mode in regular linked; do
  parent="$hook_fixture_root/$mode-parent"
  caller="$parent"
  git init --quiet --template= "$parent"
  printf 'caller repository\n' > "$parent/README.md"
  git -C "$parent" add README.md
  git -C "$parent" -c user.name=Fixture -c user.email=fixture@example.invalid -c commit.gpgsign=false commit --quiet -m initial
  if [[ $mode == linked ]]; then
    caller="$hook_fixture_root/linked-caller"
    git -C "$parent" worktree add --quiet -b linked-caller "$caller"
  fi
  git -C "$parent" config core.hooksPath "$source_root/.githooks"
  remote="$hook_fixture_root/$mode-remote.git"
  git init --quiet --bare --template= "$remote"
  before=$(git -C "$caller" rev-parse HEAD)
  index=$(git -C "$caller" rev-parse --path-format=absolute --git-path index)
  cp "$index" "$hook_fixture_root/$mode-index-before"
  PATH="$hook_fixture_root/bin:$PATH" \
    QQ_HOOK_FIXTURE="$hook_fixture_root/$mode-nested" \
    QQ_HOOK_MARKER="$hook_fixture_root/$mode-validated" \
    git -C "$caller" push --quiet "$remote" HEAD:refs/heads/verified
  if [[ $(git -C "$caller" rev-parse HEAD) != "$before" ]]; then
    echo "pre-push leaked Git environment: $mode caller HEAD changed" >&2
    exit 1
  fi
  cmp "$index" "$hook_fixture_root/$mode-index-before"
  [[ -z $(git -C "$caller" status --porcelain) ]]
  [[ $(cat "$hook_fixture_root/$mode-validated") == validated ]]
  [[ $(git -C "$hook_fixture_root/$mode-nested" rev-parse --show-toplevel) == "$hook_fixture_root/$mode-nested" ]]
  [[ $(git -C "$hook_fixture_root/$mode-nested" log -1 --format=%s) == nested ]]
  [[ $(git --git-dir="$remote" rev-parse refs/heads/verified) == "$before" ]]
  echo "pre-push: $mode caller HEAD/index preserved; nested fixture owns its commit"
done
