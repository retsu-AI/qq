#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "usage: $0 {claude-code|codex|opencode} GATEWAY_MODEL_ID JOB_NAME [harbor run args...]" >&2
  exit 2
}

[ $# -ge 3 ] || usage
harness=$1
model=$2
job=$3
shift 3

: "${LITELLM_BASE_URL:?set LITELLM_BASE_URL to the gateway's /v1 base URL}"
: "${LITELLM_API_KEY:?set LITELLM_API_KEY}"

repo=$(cd "$(dirname "$0")/../../.." && pwd)
jobs_dir=${QQ_EVAL_JOBS_DIR:-$repo/target/qq-eval/jobs}
export HARBOR_TELEMETRY=off

case "$harness" in
  claude-code)
    export ANTHROPIC_API_KEY=$LITELLM_API_KEY
    export ANTHROPIC_BASE_URL=${LITELLM_BASE_URL%/v1}
    harbor_model=$model
    ;;
  codex)
    export OPENAI_API_KEY=$LITELLM_API_KEY
    export OPENAI_BASE_URL=$LITELLM_BASE_URL
    harbor_model=openai/$model
    ;;
  opencode)
    export OPENAI_API_KEY=$LITELLM_API_KEY
    export OPENAI_BASE_URL=$LITELLM_BASE_URL
    harbor_model=openai/$model
    ;;
  *) usage ;;
esac

exec harbor run \
  --agent "$harness" \
  --model "$harbor_model" \
  --dataset terminal-bench/terminal-bench-2 \
  --job-name "$job" \
  --jobs-dir "$jobs_dir" \
  "$@"
