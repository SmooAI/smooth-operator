#!/usr/bin/env bash
# Run the smooth-operator LLM-as-judge eval suite against the live gateway, with
# the gateway virtual key fetched from @smooai/config via `th config`.
#
# This replaces the old "read the key out of opencode auth.json" step: the key is
# the single source of truth in @smooai/config, fetched at run time and never
# printed.
#
# Usage:
#   scripts/run-evals.sh                # run the default suite
#   scripts/run-evals.sh --test llm_judge -- --nocapture   # pass extra args to cargo test
#
# Env overrides:
#   SMOOAI_GATEWAY_KEY_NAME   th config key holding the sk- virtual key
#                             (default: liteLLMVirtualKeyAiServer)
#   SMOOAI_CONFIG_ENV         config environment to read (default: production)
#   SMOOTH_AGENT_JUDGE_MODEL  judge model (default: the harness CHEAP_MODEL);
#                             set e.g. claude-sonnet-4-5 for an adversarial grade
set -euo pipefail

KEY_NAME="${SMOOAI_GATEWAY_KEY_NAME:-liteLLMVirtualKeyAiServer}"
CONFIG_ENV="${SMOOAI_CONFIG_ENV:-production}"

if ! command -v th >/dev/null 2>&1; then
    echo "error: 'th' CLI not found — install the smooth CLI to fetch config secrets" >&2
    exit 1
fi

# Fetch the gateway virtual key from @smooai/config. Never echoed.
SMOOAI_GATEWAY_KEY="$(th config get "$KEY_NAME" --environment="$CONFIG_ENV" --json 2>/dev/null \
    | python3 -c 'import sys,json; print(json.load(sys.stdin).get("value",""))')"

if [ -z "${SMOOAI_GATEWAY_KEY}" ]; then
    echo "error: th config returned no value for '$KEY_NAME' (env=$CONFIG_ENV)." >&2
    echo "       Check the key name (SMOOAI_GATEWAY_KEY_NAME) and that you're logged in (th auth)." >&2
    exit 1
fi

case "$SMOOAI_GATEWAY_KEY" in
    sk-*) : ;; # ok — a LiteLLM virtual key
    *)
        echo "error: '$KEY_NAME' is not a LiteLLM virtual key (expected to start with 'sk-')." >&2
        echo "       The gateway rejects non-virtual keys with 401. Point SMOOAI_GATEWAY_KEY_NAME at a virtual key." >&2
        exit 1
        ;;
esac

export SMOOAI_GATEWAY_KEY
export SMOOTH_AGENT_E2E=1

echo "[run-evals] gateway key: $KEY_NAME (env=$CONFIG_ENV) loaded; judge=${SMOOTH_AGENT_JUDGE_MODEL:-default}"

# Default to the llm_judge suite; allow callers to override the cargo test args.
if [ "$#" -eq 0 ]; then
    set -- -p smooai-smooth-operator-evals --test llm_judge -- --nocapture --test-threads=1
fi

exec cargo test "$@"
