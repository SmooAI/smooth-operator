"""Run the local-flavor server: ``python -m smooth_operator_server``.

Boots a fully in-memory, auth-off server and serves until killed. The LLM gateway is
read from ``SMOOAI_GATEWAY_URL`` / ``SMOOAI_GATEWAY_KEY``; absent, ``send_message``
errors cleanly.

Env contract (canonical, shared with the Rust/Go/TS/.NET hosts):

``SMOOTH_AGENT_BIND``
    host to listen on (default ``127.0.0.1``)
``SMOOTH_AGENT_PORT``
    port to listen on (default ``8787``)
``SMOOTH_AGENT_SEED_KB``
    ``1`` loads the demo knowledge docs

Aliases, still honored so existing deployments keep working: ``SMOOTH_OPERATOR_BIND``
(combined ``host:port``) and ``SMOOTH_OPERATOR_SEED_KB`` — this host's pre-parity names.
The canonical name wins when both are set.

The agent gets the workspace-confined coding toolset by default (th-82ad57), sharing the
sibling hosts' env contract: ``SMOOTH_WORKSPACE`` is the root every file operation is
confined to (default: cwd), ``SMOOTH_NO_TOOLS=1`` serves a chat-only agent instead.
"""

from __future__ import annotations

import asyncio
import os
from collections.abc import Mapping

from .server import DEFAULT_HOST, DEFAULT_PORT, serve_local


def _split_addr(value: str, host: str, port: str) -> tuple[str, str]:
    """Read ``value`` as either ``host:port`` or a bare host, keeping the passed
    fallback for whichever half it does not carry."""
    head, sep, tail = value.rpartition(":")
    if sep and tail.isdigit():
        return head, tail
    return value, port


def resolve_addr(env: Mapping[str, str]) -> str:
    """Build the ``host:port`` listen address, canonical-first.

    The legacy combined name is read first so anything canonical set alongside it
    overrides it.
    """
    host, port = DEFAULT_HOST, str(DEFAULT_PORT)

    legacy = env.get("SMOOTH_OPERATOR_BIND", "").strip()
    if legacy:
        host, port = _split_addr(legacy, host, port)
    bind = env.get("SMOOTH_AGENT_BIND", "").strip()
    if bind:
        host, port = _split_addr(bind, host, port)
    explicit_port = env.get("SMOOTH_AGENT_PORT", "").strip()
    if explicit_port:
        port = explicit_port
    return f"{host}:{port}"


def resolve_seed_kb(env: Mapping[str, str]) -> bool:
    """Whether to seed the demo knowledge docs, canonical name winning over the alias."""
    raw = env.get("SMOOTH_AGENT_SEED_KB")
    if raw is None or not raw.strip():
        raw = env.get("SMOOTH_OPERATOR_SEED_KB", "")
    return raw.strip() not in ("", "0", "false", "False")


def main() -> None:
    try:
        asyncio.run(serve_local(resolve_addr(os.environ), seed_kb=resolve_seed_kb(os.environ)))
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
