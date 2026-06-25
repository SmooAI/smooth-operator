"""SmoothAgentClient — an idiomatic, transport-agnostic async client for the
smooth-operator WebSocket protocol.

Design goals (mirrors the TypeScript reference client)
------------------------------------------------------
* **Transport-agnostic.** The client never touches a real socket directly; it talks
  to an injectable :class:`~smooth_operator.transport.Transport`. The default
  (:class:`~smooth_operator.transport.WebSocketTransport`) uses `websockets`; tests
  inject an in-memory mock.
* **Request/response correlation by ``requestId``.** Every action gets a generated
  ``requestId``; the client routes incoming events back to the originating call.
* **Streaming as an async iterator.** :meth:`send_message` returns a
  :class:`MessageTurn` that is both *awaitable* (resolves with the terminal
  ``eventual_response``) and *async-iterable* (yields each ``stream_token`` /
  ``stream_chunk`` / HITL event in order). HITL resumes (``confirm_tool_action`` /
  ``verify_otp``) route back into the same turn by ``requestId``.
* **No live server required.** Correctness is fully unit-testable with a mock
  transport (see ``tests/test_client.py``).
"""

from __future__ import annotations

import asyncio
import json
import uuid
from collections.abc import AsyncIterator, Callable

from . import _generated as _g
from .transport import Transport, WebSocketTransport
from .types import (
    CreateConversationSessionResponse,
    EventualResponse,
    GetMessagesResponse,
    GetSessionResponse,
    ServerEvent,
    is_server_event,
    parse_event,
)

# Events that terminate a streaming turn (success or error).
_TURN_TERMINAL = frozenset({"eventual_response", "error"})


class ProtocolError(Exception):
    """A protocol-level ``error`` event surfaced as a raisable exception."""

    def __init__(self, code: str, message: str, request_id: str | None = None) -> None:
        super().__init__(message)
        self.code = code
        self.request_id = request_id


class RequestTimeoutError(Exception):
    """A single-response request did not receive a correlated event in time."""

    def __init__(self, request_id: str, seconds: float) -> None:
        super().__init__(f"Request {request_id} timed out after {seconds}s")
        self.request_id = request_id


class TurnTimeoutError(Exception):
    """A streaming turn received no terminal ``eventual_response`` / ``error`` within
    the configured turn timeout. The turn settles with this (``await turn`` raises it
    and ``async for`` over it re-raises it) so a stuck server can't hang the caller."""

    def __init__(self, request_id: str, seconds: float) -> None:
        super().__init__(
            f"Turn {request_id} timed out after {seconds}s without a terminal response"
        )
        self.request_id = request_id


class MessageTurn:
    """A streaming message turn.

    ``await`` it for the terminal :class:`EventualResponse`, or ``async for`` over it
    to receive every intermediate event in arrival order::

        turn = client.send_message(session_id=sid, message="hi")
        async for event in turn:
            if event.type == "stream_token":
                print(event.token, end="")
        final = await turn  # EventualResponse

    Both consumption styles can run concurrently. Buffered events delivered before
    iteration begins are preserved.
    """

    def __init__(
        self,
        request_id: str,
        on_close: Callable[[], None],
        turn_timeout: float = 0.0,
    ) -> None:
        self.request_id = request_id
        self._on_close = on_close
        self._turn_timeout = turn_timeout
        # asyncio.Queue / asyncio.Event bind to the running loop lazily on first use,
        # so they need no explicit loop capture. The settled future, however, is bound
        # at construction — use get_running_loop() so it attaches to the loop that is
        # actually running (the client is async-only; send_message is always called
        # from within a running loop). get_event_loop() could otherwise return / create
        # a *different* loop that never runs, leaving the future (and any `await turn`)
        # to hang silently.
        self._queue: asyncio.Queue[ServerEvent] = asyncio.Queue()
        self._done = asyncio.Event()
        self._final: EventualResponse | None = None
        self._error: BaseException | None = None
        loop = asyncio.get_running_loop()
        self._settled: asyncio.Future[EventualResponse] = loop.create_future()
        # Avoid "Future exception was never retrieved" noise when the caller only
        # iterates (and surfaces the error via __aiter__) and never awaits the turn.
        self._settled.add_done_callback(lambda f: f.cancelled() or f.exception())
        # Bound the turn: a server that accepts send_message but never emits a terminal
        # event must not hang the caller forever.
        self._timeout_handle: asyncio.TimerHandle | None = None
        if turn_timeout > 0:
            self._timeout_handle = loop.call_later(turn_timeout, self._on_timeout)

    # ── feed (called by the client dispatcher) ─────────────────────────────────
    def push(self, event: ServerEvent) -> None:
        if self._done.is_set():
            return

        if event.type == "error":
            self._queue.put_nowait(event)
            code, message = _extract_error(event)
            self._finish(None, ProtocolError(code, message, self.request_id))
            return

        self._queue.put_nowait(event)

        if event.type == "eventual_response":
            self._finish(event, None)

    def abort(self, err: BaseException) -> None:
        """Force-close the turn (e.g. on disconnect)."""
        if self._done.is_set():
            return
        self._finish(None, err)

    def _on_timeout(self) -> None:
        """Settle the turn with a TurnTimeoutError when no terminal event arrived."""
        if self._done.is_set():
            return
        self._finish(
            None, TurnTimeoutError(self.request_id, self._turn_timeout)
        )

    def _finish(self, final: EventualResponse | None, err: BaseException | None) -> None:
        if self._done.is_set():
            return
        if self._timeout_handle is not None:
            self._timeout_handle.cancel()
            self._timeout_handle = None
        self._final = final
        self._error = err
        self._done.set()
        self._on_close()
        if not self._settled.done():
            if err is not None:
                self._settled.set_exception(err)
            elif final is not None:
                self._settled.set_result(final)

    # ── async iteration ────────────────────────────────────────────────────────
    def __aiter__(self) -> AsyncIterator[ServerEvent]:
        return self._iterate()

    async def _iterate(self) -> AsyncIterator[ServerEvent]:
        while True:
            if not self._queue.empty():
                yield self._queue.get_nowait()
                continue
            if self._done.is_set():
                # Drain anything that raced in just before done was set.
                while not self._queue.empty():
                    yield self._queue.get_nowait()
                if self._error is not None:
                    raise self._error
                return
            # Wait for either a new event or the turn to finish.
            get_task = asyncio.ensure_future(self._queue.get())
            done_task = asyncio.ensure_future(self._done.wait())
            try:
                await asyncio.wait(
                    {get_task, done_task}, return_when=asyncio.FIRST_COMPLETED
                )
            finally:
                if not get_task.done():
                    get_task.cancel()
                if not done_task.done():
                    done_task.cancel()
            if get_task.done() and not get_task.cancelled():
                yield get_task.result()

    # ── awaitable (resolves with the EventualResponse) ─────────────────────────
    def __await__(self):
        return self._settled.__await__()

    async def result(self) -> EventualResponse:
        """Await the terminal :class:`EventualResponse` (or raise ProtocolError)."""
        return await self._settled


class SmoothAgentClient:
    """Async client over an injectable :class:`Transport`."""

    def __init__(
        self,
        url: str,
        *,
        token: str | None = None,
        transport: Transport | None = None,
        generate_request_id: Callable[[], str] | None = None,
        request_timeout: float = 30.0,
        turn_timeout: float = 120.0,
    ) -> None:
        # ``token`` authenticates against a token-gated (local-flavor) server: it is
        # folded into the connection URL's ``?token=`` slot on the default transport.
        # A custom ``transport`` is used as-is (apply the token to its own URL there).
        self._transport = (
            transport if transport is not None else WebSocketTransport(url, token=token)
        )
        self._request_timeout = request_timeout
        # Overall timeout (seconds) for a streaming send_message turn. 0 disables it.
        self._turn_timeout = turn_timeout
        self._generate_request_id = generate_request_id or (lambda: f"req-{uuid.uuid4()}")

        # requestId → Future for single-response requests (create_session, ping, …).
        self._pending: dict[str, asyncio.Future[ServerEvent]] = {}
        # requestId → active streaming turn (send_message + HITL resumes).
        self._turns: dict[str, MessageTurn] = {}
        # Unsolicited-event listeners (keepalive, server-push).
        self._listeners: set[Callable[[ServerEvent], None]] = set()

        self._unsubscribe: list[Callable[[], None]] = [
            self._transport.on_message(self._handle_frame),
            self._transport.on_close(
                lambda _info: self._fail_all(ConnectionError("Transport closed"))
            ),
        ]

    # ── lifecycle ──────────────────────────────────────────────────────────────
    async def connect(self) -> None:
        await self._transport.connect()

    async def disconnect(self, reason: str = "client disconnect") -> None:
        self._fail_all(ConnectionError(reason))
        for unsub in self._unsubscribe:
            unsub()
        self._unsubscribe = []
        await self._transport.close(1000, reason)

    def on_event(self, listener: Callable[[ServerEvent], None]) -> Callable[[], None]:
        """Subscribe to unsolicited / uncorrelated server events (e.g. keepalive)."""
        self._listeners.add(listener)
        return lambda: self._listeners.discard(listener)

    # ── actions ──────────────────────────────────────────────────────────────
    async def create_conversation_session(
        self,
        *,
        agent_id: str,
        user_name: str | None = None,
        user_email: str | None = None,
        browser_fingerprint: str | None = None,
        metadata: dict | None = None,
        auth_context: dict | None = None,
    ) -> CreateConversationSessionResponse:
        """Start a new conversation session. Resolves with the session descriptor."""
        frame: dict = {"action": "create_conversation_session", "agentId": agent_id}
        if user_name is not None:
            frame["userName"] = user_name
        if user_email is not None:
            frame["userEmail"] = user_email
        if browser_fingerprint is not None:
            frame["browserFingerprint"] = browser_fingerprint
        if metadata is not None:
            frame["metadata"] = metadata
        if auth_context is not None:
            frame["authContext"] = auth_context
        event = await self._request(frame)
        return CreateConversationSessionResponse.model_validate(
            _immediate_data(event)
        )

    async def get_session(self, *, session_id: str) -> GetSessionResponse:
        """Fetch a session snapshot by ID."""
        event = await self._request({"action": "get_session", "sessionId": session_id})
        return GetSessionResponse.model_validate(_immediate_data(event))

    async def get_messages(
        self,
        *,
        session_id: str,
        limit: int | None = None,
        before: str | None = None,
    ) -> GetMessagesResponse:
        """Fetch a page of conversation messages."""
        frame: dict = {"action": "get_conversation_messages", "sessionId": session_id}
        if limit is not None:
            frame["limit"] = limit
        if before is not None:
            frame["before"] = before
        event = await self._request(frame)
        return GetMessagesResponse.model_validate(_immediate_data(event))

    async def ping(self) -> int:
        """Keepalive ping. Resolves with the server timestamp from the ``pong``."""
        event = await self._request({"action": "ping"})
        if event.type == "pong":
            if event.timestamp is not None:
                return event.timestamp
            if event.data is not None:
                return event.data.timestamp
        return 0

    def send_message(
        self, *, session_id: str, message: str, stream: bool = True
    ) -> MessageTurn:
        """Submit a user message and return a :class:`MessageTurn`.

        Await it for the terminal ``eventual_response``, or ``async for`` over it for
        the streaming events. Synchronous (non-awaiting) — the turn is returned
        immediately so the caller can start iterating.
        """
        request_id = self._generate_request_id()
        turn = MessageTurn(
            request_id,
            lambda: self._turns.pop(request_id, None),
            turn_timeout=self._turn_timeout,
        )
        self._turns[request_id] = turn
        try:
            self._transport.send(
                json.dumps(
                    {
                        "action": "send_message",
                        "requestId": request_id,
                        "sessionId": session_id,
                        "message": message,
                        "stream": stream,
                    }
                )
            )
        except Exception as err:  # noqa: BLE001 - surface send failure to the turn
            self._turns.pop(request_id, None)
            turn.abort(err)
        return turn

    def confirm_tool_action(
        self, *, session_id: str, request_id: str, approved: bool
    ) -> None:
        """Approve/reject a pending tool write, resuming the paused turn for
        ``request_id``. Resumed events flow back into the original :class:`MessageTurn`."""
        self._transport.send(
            json.dumps(
                {
                    "action": "confirm_tool_action",
                    "sessionId": session_id,
                    "requestId": request_id,
                    "approved": approved,
                }
            )
        )

    def verify_otp(self, *, session_id: str, request_id: str, code: str) -> None:
        """Submit an OTP code, resuming the paused turn for ``request_id``. Resumed
        events flow back into the original :class:`MessageTurn`."""
        self._transport.send(
            json.dumps(
                {
                    "action": "verify_otp",
                    "sessionId": session_id,
                    "requestId": request_id,
                    "code": code,
                }
            )
        )

    # ── internals ──────────────────────────────────────────────────────────────
    async def _request(self, action: dict) -> ServerEvent:
        request_id = action.get("requestId") or self._generate_request_id()
        frame = {**action, "requestId": request_id}
        # Bind the response future to the *running* loop (the client is async-only and
        # _request is always awaited). get_event_loop() can hand back a non-running
        # loop, leaving this future to never resolve — a silent hang.
        loop = asyncio.get_running_loop()
        fut: asyncio.Future[ServerEvent] = loop.create_future()
        self._pending[request_id] = fut

        try:
            self._transport.send(json.dumps(frame))
        except Exception:
            self._pending.pop(request_id, None)
            raise

        try:
            if self._request_timeout > 0:
                return await asyncio.wait_for(fut, timeout=self._request_timeout)
            return await fut
        except asyncio.TimeoutError as exc:
            self._pending.pop(request_id, None)
            raise RequestTimeoutError(request_id, self._request_timeout) from exc

    def _handle_frame(self, data: str) -> None:
        try:
            raw = json.loads(data)
        except (ValueError, TypeError):
            return  # ignore malformed frames
        if not is_server_event(raw):
            return
        try:
            event = parse_event(raw)
        except Exception:  # noqa: BLE001 - drop frames that fail schema validation
            return
        request_id = event.request_id

        # 1. Streaming turn? Route every related event into it.
        if request_id and request_id in self._turns:
            self._turns[request_id].push(event)
            return

        # 2. Single-response request awaiting resolution?
        if request_id and request_id in self._pending:
            fut = self._pending.pop(request_id)
            if fut.done():
                return
            if event.type == "error":
                code, message = _extract_error(event)
                fut.set_exception(ProtocolError(code, message, request_id))
            else:
                fut.set_result(event)
            return

        # 3. Unsolicited / uncorrelated event (keepalive, server push).
        for listener in list(self._listeners):
            listener(event)

    def _fail_all(self, err: BaseException) -> None:
        for fut in list(self._pending.values()):
            if not fut.done():
                fut.set_exception(err)
        self._pending.clear()
        for turn in list(self._turns.values()):
            turn.abort(err)
        self._turns.clear()


def _immediate_data(event: ServerEvent) -> dict:
    """Pull the typed ``data`` payload out of an ``immediate_response`` event."""
    if event.type == "immediate_response":
        return event.data
    data = getattr(event, "data", None)
    if isinstance(data, dict):
        return data
    raise ProtocolError(
        "UNEXPECTED_EVENT",
        f'Expected immediate_response, got "{event.type}"',
        event.request_id,
    )


def _extract_error(event: _g.Error) -> tuple[str, str]:
    """Pull ``(code, message)`` out of an ``error`` event, preferring the nested
    ``data.error`` shape and falling back to the envelope-level ``error``."""
    err = None
    data = getattr(event, "data", None)
    if data is not None and getattr(data, "error", None) is not None:
        err = data.error
    elif getattr(event, "error", None) is not None:
        err = event.error
    if err is not None:
        return err.code, err.message
    return "INTERNAL_ERROR", "Unknown protocol error"
