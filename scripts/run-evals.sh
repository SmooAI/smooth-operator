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
#                             (default: smooaiLlmKey — the smooai org's own LLM
#                             virtual key, mirror of org_llm_keys; secret tier)
#   SMOOAI_EVAL_GATEWAY_ENV   config environment the gateway key lives in
#                             (default: production). Deliberately NOT the ambient
#                             SMOOAI_CONFIG_ENV — that's your local working env
#                             (often `development`, where the key is just a
#                             placeholder), unrelated to where the prod key lives.
#   SMOOAI_CONFIG_ORG_ID      org whose config holds the key. `th config` reads
#                             the org from this env (set by direnv in the smooai
#                             monorepo); without it, th resolves a different/default
#                             org and returns the wrong value. Export it (or source
#                             the monorepo's .envrc) before running outside that repo.
#   SMOOTH_AGENT_JUDGE_MODEL  judge model (default: the harness CHEAP_MODEL);
#                             set e.g. claude-sonnet-4-5 for an adversarial grade
set -euo pipefail

KEY_NAME="${SMOOAI_GATEWAY_KEY_NAME:-smooaiLlmKey}"
CONFIG_ENV="${SMOOAI_EVAL_GATEWAY_ENV:-production}"

if ! command -v th >/dev/null 2>&1; then
    echo "error: 'th' CLI not found — install the smooth CLI to fetch config secrets" >&2
    exit 1
fi

# The key lives in a specific org (SmooAI's infra-secrets / master org). We pin it
# explicitly and authenticate via the `th` user JWT, deliberately UNSETTING any
# ambient @smooai/config M2M env vars (SMOOAI_CONFIG_API_KEY / CLIENT_*) for the
# fetch — those are scoped to whatever org the surrounding direnv loaded and would
# otherwise override --org-id and return the wrong value.
if [ -z "${SMOOAI_CONFIG_ORG_ID:-}" ]; then
    echo "error: SMOOAI_CONFIG_ORG_ID is not set." >&2
    echo "       Set it to the org that holds '$KEY_NAME' (SmooAI's infra-secrets org)," >&2
    echo "       e.g. export SMOOAI_CONFIG_ORG_ID=<org-uuid>, then re-run." >&2
    exit 1
fi

# Fetch the gateway virtual key from @smooai/config via the user JWT. Never echoed.
SMOOAI_GATEWAY_KEY="$(env -u SMOOAI_CONFIG_API_KEY -u SMOOAI_CONFIG_CLIENT_ID -u SMOOAI_CONFIG_CLIENT_SECRET -u SMOOAI_CONFIG_API_URL \
    th config get "$KEY_NAME" --environment="$CONFIG_ENV" --org-id "$SMOOAI_CONFIG_ORG_ID" --json 2>/dev/null \
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
