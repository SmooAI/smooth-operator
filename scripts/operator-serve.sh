#!/usr/bin/env bash
# operator-serve.sh <lang> [port] — run a smooth-operator server as "Big Smooth":
# the shared smooth-web Presence UI, same-origin, token-auth'd, with the Big Smooth
# persona. The protocol is conformance-identical, so the same UI drives any backend.
#
# Wiring a new language? It must honor SMOOTH_WEB_DIR + SMOOTH_LOCAL_TOKEN +
# SMOOTH_PERSONA (see dotnet/server for the reference) — then flip its case below
# from the "not wired" guard to its run command. Use /polyglot-parity.
set -euo pipefail

lang="${1:?usage: operator-serve.sh <rust|csharp|python|typescript|go> [port]}"
port="${2:-8787}"
repo="$(cd "$(dirname "$0")/.." && pwd)"

# The Presence SPA dist (smooth-web). Override with SMOOTH_WEB_DIR; default to a
# sibling `smooth` checkout. Must be a built dist (run `pnpm build` in smooth-web/web).
web_dir="${SMOOTH_WEB_DIR:-$repo/../smooth/crates/smooth-web/web/dist}"
if [[ ! -f "$web_dir/index.html" ]]; then
  echo "no Presence dist at $web_dir — set SMOOTH_WEB_DIR or build smooth-web/web (pnpm build)" >&2
  exit 1
fi

# Gateway creds: env first, else the smooth provider key from `th auth login smooth`.
gateway_url="${SMOOTH_GATEWAY_URL:-https://llm.smoo.ai/v1}"
gateway_key="${SMOOTH_GATEWAY_KEY:-$(python3 -c 'import json,os;d=json.load(open(os.path.expanduser("~/.smooth/providers.json")));print(next(p["api_key"] for p in d["providers"] if p["id"]=="smooth"))' 2>/dev/null || true)}"
model="${SMOOTH_MODEL:-deepseek-v4-flash}"
token="${SMOOTH_LOCAL_TOKEN:-$(openssl rand -hex 16)}"
persona="${SMOOTH_PERSONA:-You are Big Smooth, your humans always-on personal AI operator. Speak plainly and warmly, first person. You are not a customer-support bot. Do not narrate chain-of-thought.}"

export SMOOTH_WEB_DIR="$web_dir" SMOOTH_LOCAL_TOKEN="$token" SMOOTH_PERSONA="$persona"
export SMOOTH_GATEWAY_URL="$gateway_url" SMOOTH_GATEWAY_KEY="$gateway_key" SMOOTH_MODEL="$model"

echo "Big Smooth ($lang) → http://127.0.0.1:$port/   model=$model   token=$token"

not_wired() { echo "the $1 server does not honor the Big Smooth env contract yet (SMOOTH_WEB_DIR/_LOCAL_TOKEN/_PERSONA). Wire it like dotnet/server, then add its run command here. See /polyglot-parity." >&2; exit 2; }

case "$lang" in
  rust)   echo "rust is the native daemon — run 'th daemon' (embeds the SPA + persona)."; exit 0 ;;
  csharp)
    dll="$repo/dotnet/server/host/bin/Release/net8.0/SmooAI.SmoothOperator.Server.Host.dll"
    [[ -f "$dll" ]] || dotnet build "$repo/dotnet/server/host/SmooAI.SmoothOperator.Server.Host.csproj" -c Release >&2
    ASPNETCORE_URLS="http://127.0.0.1:$port" exec dotnet "$dll" ;;
  python)     not_wired python ;;
  typescript) not_wired typescript ;;
  go)         not_wired go ;;
  *) echo "unknown lang: $lang" >&2; exit 1 ;;
esac
