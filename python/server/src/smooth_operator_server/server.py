"""The asyncio WebSocket server: one ``/ws``-style endpoint, one task per
connection.

Per connection we run a read loop and a single outbound **writer** task joined by
an :class:`asyncio.Queue` outbound sink — ``websockets`` ``send`` isn't safe to
call concurrently from many producers, so all events funnel through one writer
(mirrors the Rust ``sink_tx`` + writer split and the C# channel + writer task).

Graceful SIGTERM drain (the spec): a shared :class:`asyncio.Event` cancel switch
lives on :class:`ServerState`. Each connection loop, on every iteration, checks the
cancel FIRST (prefer cancel on ties), then races "cancel set" vs "next inbound
frame"; the turn dispatch is awaited INSIDE the frame branch so an in-flight turn
finishes before the loop exits. ``SIGTERM`` / ``SIGINT`` handlers stop accepting new
connections and set the cancel. A backplane ``detach`` always runs after the loop
exits (detach-after-loop).
"""

from __future__ import annotations

import asyncio
import json
import os
import signal
import uuid
from dataclasses import dataclass, field
from typing import Any, Optional
from urllib.parse import parse_qs, urlsplit

import websockets
from smooth_operator_core import Knowledge

from .agent_config import AgentConfigResolver, NoSessionAuthenticator, SessionAuthenticator, StaticAgentConfigResolver
from .auth import AccessContext, AuthVerifier, NoAuthVerifier
from .backplane import Backplane, InMemoryBackplane
from .confirmation import ConfirmationRegistry
from .dispatcher import FrameDispatcher
from .session_store import InMemorySessionStore, SessionStore
from .workflow import WORKFLOW_JUDGE_MODEL

#: Default loopback bind, matching the Rust local flavor's canonical WS port.
DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 8787


@dataclass
class ServerState:
    """Everything a connection needs, plus the shared graceful-drain switch.

    ``cancel`` is the ONE source of truth for "stop": SIGTERM/SIGINT set it, each
    connection loop watches it. It defaults to UNSET (the server runs)."""

    store: SessionStore
    chat_client: Any = None
    knowledge: Knowledge | None = None
    auth: AuthVerifier = field(default_factory=NoAuthVerifier)
    backplane: Backplane = field(default_factory=InMemoryBackplane)
    system_prompt: str | None = None
    model: str | None = None
    #: Tools the agent may call during a turn (default none). Each is an engine
    #: ``FunctionTool``/``Tool``; the turn runner passes them straight to the agent.
    tools: list[Any] = field(default_factory=list)
    #: Tool-name patterns gated behind write-confirmation HITL (default empty → no
    #: gating, behavior unchanged). When a turn calls a tool whose name contains one
    #: of these, the server parks the turn and emits ``write_confirmation_required``
    #: until the client replies with ``confirm_tool_action``.
    confirm_tools: list[str] = field(default_factory=list)
    #: Per-agent config resolver (instructions / conversation workflow / persona),
    #: keyed by ``agentId`` (SMOODEV-590). The config-delivery seam: resolved per turn
    #: from the session's agent — the default (empty static resolver) returns ``None``
    #: for every agent, so behavior is unchanged. A multi-tenant host swaps in a
    #: resolver backed by the `agents` table.
    agent_config_resolver: AgentConfigResolver = field(default_factory=StaticAgentConfigResolver)
    #: Seam deciding whether a conversation's user is identity-verified — gates
    #: ``end_user`` auth-level tools on public agents. Default fails closed.
    session_authenticator: SessionAuthenticator = field(default_factory=NoSessionAuthenticator)
    #: Fast/cheap model for the post-turn workflow judge (default haiku-tier).
    judge_model: str = WORKFLOW_JUDGE_MODEL
    cancel: asyncio.Event = field(default_factory=asyncio.Event)


def _resolve_access(state: ServerState, path: str) -> AccessContext:
    """Resolve the connection's :class:`AccessContext` from the ``?token=`` query
    slot (browsers can't set WS headers). Fail-closed to anonymous on any problem."""
    try:
        query = parse_qs(urlsplit(path).query)
        token = query.get("token", [None])[0]
    except Exception:
        token = None
    return state.auth.resolve(token)


async def _connection_loop(websocket: Any, state: ServerState, access: AccessContext) -> None:
    """Drive one WebSocket connection: a writer task draining an outbound queue +
    a read loop racing the cancel switch against the next inbound frame.

    The detach-after-loop runs in ``finally`` so the backplane deregister happens
    whether the loop exits on close, cancel, or error."""
    conn_id = str(uuid.uuid4())
    await state.backplane.attach(conn_id)

    outbound: asyncio.Queue[Optional[dict[str, Any]]] = asyncio.Queue()

    async def writer() -> None:
        # Drain the queue and write each event as a JSON text frame. A sentinel
        # `None` ends the writer cleanly after the read loop finishes.
        while True:
            event = await outbound.get()
            if event is None:
                return
            try:
                await websocket.send(json.dumps(event))
            except websockets.ConnectionClosed:
                return

    writer_task = asyncio.create_task(writer())

    def sink(event: dict[str, Any]) -> None:
        # Sync enqueue (the dispatcher's sink is sync); the writer task drains it.
        outbound.put_nowait(event)

    # One pending-confirmation registry per connection: a `confirm_tool_action`
    # frame and the parked turn it resumes are always on the same connection (the
    # session id keys within it), so the registry need not be server-wide.
    confirmations = ConfirmationRegistry()
    dispatcher = FrameDispatcher(
        state.store,
        state.chat_client,
        knowledge=state.knowledge,
        access=access,
        system_prompt=state.system_prompt,
        model=state.model,
        tools=state.tools,
        confirm_tools=state.confirm_tools,
        confirmations=confirmations,
        agent_config_resolver=state.agent_config_resolver,
        session_authenticator=state.session_authenticator,
        judge_model=state.judge_model,
    )

    cancel_wait = asyncio.ensure_future(state.cancel.wait())
    try:
        while True:
            # Check the cancel FIRST each iteration — prefer cancel on ties so a
            # drain that arrives between frames stops us before the next read.
            if state.cancel.is_set():
                break

            recv_task = asyncio.ensure_future(websocket.recv())
            done, _ = await asyncio.wait(
                {recv_task, cancel_wait},
                return_when=asyncio.FIRST_COMPLETED,
            )

            if recv_task in done:
                try:
                    raw = recv_task.result()
                except websockets.ConnectionClosed:
                    break
                # Dispatch the turn INSIDE the frame branch and AWAIT it, so an
                # in-flight turn finishes even if the cancel fires mid-turn.
                if isinstance(raw, bytes):
                    raw = raw.decode("utf-8")
                await dispatcher.dispatch(raw, sink)
            else:
                # Cancel won the race (no frame in flight) → stop accepting.
                recv_task.cancel()
                break
    finally:
        cancel_wait.cancel()
        # Any turn parked on a write-confirmation must unpark before we can finish:
        # reject outstanding confirmations (fail closed — a write is never auto-
        # approved on disconnect), then await every in-flight spawned turn so its
        # `eventual_response` is enqueued before the writer stops (preserves the
        # graceful-drain "in-flight turn finishes" contract now that turns run as
        # background tasks rather than inline).
        confirmations.reject_all()
        await dispatcher.wait_for_turns()
        # Stop the writer (drain any already-queued events first), then detach —
        # the detach-after-loop runs regardless of how the loop exited.
        outbound.put_nowait(None)
        try:
            await writer_task
        except asyncio.CancelledError:
            pass
        await state.backplane.detach(conn_id)


class Server:
    """A running smooth-operator WebSocket server with a graceful-drain switch."""

    def __init__(self, state: ServerState, ws_server: Any) -> None:
        self._state = state
        self._ws_server = ws_server

    @property
    def state(self) -> ServerState:
        return self._state

    @property
    def host(self) -> str:
        return self._ws_server.sockets[0].getsockname()[0]

    @property
    def port(self) -> int:
        return self._ws_server.sockets[0].getsockname()[1]

    def ws_url(self) -> str:
        return f"ws://{self.host}:{self.port}/ws"

    def drain(self) -> None:
        """Signal graceful shutdown: stop accepting new connections + set the
        cancel so each connection loop exits after any in-flight turn finishes."""
        self._state.cancel.set()
        self._ws_server.close()

    async def wait_closed(self) -> None:
        await self._ws_server.wait_closed()

    async def shutdown(self) -> None:
        """Drain and await a clean exit."""
        self.drain()
        await self.wait_closed()


async def serve(
    state: ServerState,
    host: str = DEFAULT_HOST,
    port: int = DEFAULT_PORT,
    *,
    install_signal_handlers: bool = False,
) -> Server:
    """Bind and start serving (returns once the listener is up — does NOT block).

    The handler resolves each connection's access from its ``?token=`` slot, then
    runs the per-connection loop. With ``install_signal_handlers``, SIGTERM/SIGINT
    trigger a graceful drain."""

    async def handler(websocket: Any) -> None:
        # The request target (`/ws?token=...`) carries the auth token in its query
        # slot — browsers can't set custom WS handshake headers. `websockets` >=12
        # exposes it as `websocket.request.path`; older versions used
        # `websocket.path`. Read whichever is present.
        request = getattr(websocket, "request", None)
        path = getattr(request, "path", None) or getattr(websocket, "path", "") or ""
        access = _resolve_access(state, path)
        await _connection_loop(websocket, state, access)

    ws_server = await websockets.serve(handler, host, port)
    server = Server(state, ws_server)

    if install_signal_handlers:
        loop = asyncio.get_running_loop()
        for sig in (signal.SIGTERM, signal.SIGINT):
            try:
                loop.add_signal_handler(sig, server.drain)
            except (NotImplementedError, RuntimeError):
                # Signal handlers aren't available on every platform / loop; skip.
                pass

    return server


def _build_gateway_client() -> Any:
    """Build the live OpenAI-compatible async client against the SmooAI gateway
    from ``SMOOAI_GATEWAY_URL`` / ``SMOOAI_GATEWAY_KEY``. Returns ``None`` when no
    key is set (the server then serves protocol-only; ``send_message`` errors
    cleanly with ``LLM_UNAVAILABLE``). Mirrors the Rust ``config.llm_config()`` gate."""
    key = os.environ.get("SMOOAI_GATEWAY_KEY")
    if not key:
        return None
    try:
        from openai import AsyncOpenAI
    except ImportError as exc:  # pragma: no cover - env without the gateway extra
        raise RuntimeError(
            "SMOOAI_GATEWAY_KEY is set but the 'openai' client is not installed; "
            "install with the [gateway] extra to enable live turns."
        ) from exc
    base_url = os.environ.get("SMOOAI_GATEWAY_URL")
    return AsyncOpenAI(api_key=key, base_url=base_url) if base_url else AsyncOpenAI(api_key=key)


async def serve_local(addr: str = f"{DEFAULT_HOST}:{DEFAULT_PORT}", *, seed_kb: bool = False) -> None:
    """The **local deployment flavor** — an embeddable, zero-config server that
    blocks until killed.

    Everything in-memory, auth off, loopback bind — needs no external services and
    no secrets (mirrors the Rust ``local::serve_local``). The LLM gateway is still
    read from the environment, so a key present enables live turns; absent,
    ``send_message`` returns a clean protocol error.

    For an embeddable handle you can stop programmatically, use :func:`serve` with a
    :class:`ServerState` and call :meth:`Server.shutdown`."""
    host, _, port_str = addr.partition(":")
    port = int(port_str) if port_str else DEFAULT_PORT

    store = InMemorySessionStore()
    knowledge: Knowledge | None = None
    if seed_kb:
        knowledge = _seed_knowledge()

    state = ServerState(
        store=store,
        chat_client=_build_gateway_client(),
        knowledge=knowledge,
        auth=NoAuthVerifier(),
    )
    server = await serve(state, host, port, install_signal_handlers=True)
    print(f"smooth-operator-server (local flavor, python) listening on {server.ws_url()}")
    await server.wait_closed()


def _seed_knowledge() -> Knowledge:
    """Seed a couple of distinctive demo docs so a knowledge-grounded turn is
    deterministic. The 17-day return window is deliberately unusual (mirrors the
    Rust ``seed_knowledge``)."""
    from smooth_operator_core import InMemoryKnowledge

    kb = InMemoryKnowledge()
    kb.ingest(
        "SmooAI's return window is exactly 17 days from delivery. Returns after 17 days are not accepted.",
        source="policies/returns.md",
    )
    kb.ingest(
        "SmooAI standard shipping takes 5 to 7 business days. Expedited shipping takes 2 business days.",
        source="policies/shipping.md",
    )
    return kb
