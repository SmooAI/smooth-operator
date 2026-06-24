"""Native async WebSocket server for the smooth-operator protocol.

A parity implementation of the Rust (``rust/smooth-operator-server``) and C#
(``dotnet/server``) reference servers, consuming the in-process
``smooai-smooth-operator-core`` engine. The server runs a :class:`SmoothAgent` per
turn and maps its stream events onto the wire protocol's ``stream_token`` /
``stream_chunk`` / ``eventual_response`` events.

Quick start (embeddable, in-memory, auth off)::

    import asyncio
    from smooth_operator_server import serve_local

    asyncio.run(serve_local("127.0.0.1:8787", seed_kb=True))
"""

from __future__ import annotations

from . import protocol
from .auth import (
    AccessContext,
    AuthVerifier,
    LocalTokenVerifier,
    NoAuthVerifier,
    Principal,
)
from .backplane import Backplane, InMemoryBackplane
from .dispatcher import FrameDispatcher
from .server import (
    DEFAULT_HOST,
    DEFAULT_PORT,
    Server,
    ServerState,
    serve,
    serve_local,
)
from .session_store import (
    InMemorySessionStore,
    MessageDirection,
    SessionStore,
    StoredMessage,
    StoredSession,
)
from .turn_runner import TurnResult, TurnRunner

__all__ = [
    "protocol",
    "AccessContext",
    "AuthVerifier",
    "LocalTokenVerifier",
    "NoAuthVerifier",
    "Principal",
    "Backplane",
    "InMemoryBackplane",
    "FrameDispatcher",
    "DEFAULT_HOST",
    "DEFAULT_PORT",
    "Server",
    "ServerState",
    "serve",
    "serve_local",
    "InMemorySessionStore",
    "MessageDirection",
    "SessionStore",
    "StoredMessage",
    "StoredSession",
    "TurnResult",
    "TurnRunner",
]
