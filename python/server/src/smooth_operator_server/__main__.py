"""Run the local-flavor server: ``python -m smooth_operator_server``.

Boots a fully in-memory, auth-off server on ``SMOOTH_OPERATOR_BIND`` (default
``127.0.0.1:8787``) and serves until killed. ``SMOOTH_OPERATOR_SEED_KB=1`` loads the
demo knowledge docs. The LLM gateway is read from ``SMOOAI_GATEWAY_URL`` /
``SMOOAI_GATEWAY_KEY``; absent, ``send_message`` errors cleanly.

The agent gets the workspace-confined coding toolset by default (th-82ad57), sharing the
sibling hosts' env contract: ``SMOOTH_WORKSPACE`` is the root every file operation is
confined to (default: cwd), ``SMOOTH_NO_TOOLS=1`` serves a chat-only agent instead.
"""

from __future__ import annotations

import asyncio
import os

from .server import DEFAULT_HOST, DEFAULT_PORT, serve_local


def main() -> None:
    addr = os.environ.get("SMOOTH_OPERATOR_BIND", f"{DEFAULT_HOST}:{DEFAULT_PORT}")
    seed_kb = os.environ.get("SMOOTH_OPERATOR_SEED_KB", "") not in ("", "0", "false", "False")
    try:
        asyncio.run(serve_local(addr, seed_kb=seed_kb))
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
