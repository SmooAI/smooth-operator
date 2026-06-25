"""Transport abstraction for the async client.

The client is deliberately decoupled from any concrete WebSocket implementation so
it can be unit-tested with a mock and run against any socket. A :class:`Transport`
is anything that can send a string frame and surface incoming string frames plus
lifecycle (close/error) callbacks.

The default :class:`WebSocketTransport` uses the optional `websockets` library; if
it is not installed the constructor raises with a clear message, and callers can
inject any :class:`Transport` instead (e.g. the in-memory mock used in tests).
"""

from __future__ import annotations

import asyncio
from abc import ABC, abstractmethod
from collections.abc import Callable
from typing import Literal
from urllib.parse import urlencode, urlsplit, urlunsplit

TransportState = Literal["connecting", "open", "closing", "closed"]

CloseInfo = dict  # {"code": int | None, "reason": str | None}
MessageHandler = Callable[[str], None]
CloseHandler = Callable[[CloseInfo], None]
ErrorHandler = Callable[[object], None]


def apply_connection_token(url: str, token: str | None) -> str:
    """Return ``url`` with a ``token`` query parameter for a token-gated server.

    Token-gated (local-flavor) servers read the connection token from the
    ``?token=`` query slot of the WS URL. The token is merged into any existing
    query string via :func:`urllib.parse.urlsplit` / :func:`urlencode` (never a
    naive concat), so an existing ``?foo=bar`` is preserved. ``None`` returns the
    URL unchanged so the default (no-token) behavior is untouched.
    """
    if token is None:
        return url
    parts = urlsplit(url)
    # parse_qsl drops the token if it's already present; rebuild deterministically.
    query = [(k, v) for k, v in _parse_query(parts.query) if k != "token"]
    query.append(("token", token))
    return urlunsplit(parts._replace(query=urlencode(query)))


def _parse_query(query: str) -> list[tuple[str, str]]:
    from urllib.parse import parse_qsl  # noqa: PLC0415

    # keep_blank_values so a bare ``?foo=`` round-trips instead of vanishing.
    return parse_qsl(query, keep_blank_values=True)


class Transport(ABC):
    """Minimal injectable transport contract (mirrors a WebSocket subset)."""

    @property
    @abstractmethod
    def state(self) -> TransportState: ...

    @abstractmethod
    async def connect(self) -> None:
        """Open the connection. Resolves once the transport reaches ``open``."""

    @abstractmethod
    def send(self, data: str) -> None:
        """Send a serialized frame. Raises if the transport is not open."""

    @abstractmethod
    async def close(self, code: int = 1000, reason: str = "") -> None:
        """Close the connection."""

    @abstractmethod
    def on_message(self, handler: MessageHandler) -> Callable[[], None]:
        """Register a handler for incoming string frames. Returns an unsubscribe fn."""

    @abstractmethod
    def on_close(self, handler: CloseHandler) -> Callable[[], None]:
        """Register a handler for transport close. Returns an unsubscribe fn."""

    @abstractmethod
    def on_error(self, handler: ErrorHandler) -> Callable[[], None]:
        """Register a handler for transport-level errors. Returns an unsubscribe fn."""


class _HandlerMixin:
    """Shared registry plumbing for concrete transports."""

    def __init__(self) -> None:
        self._message_handlers: set[MessageHandler] = set()
        self._close_handlers: set[CloseHandler] = set()
        self._error_handlers: set[ErrorHandler] = set()

    def on_message(self, handler: MessageHandler) -> Callable[[], None]:
        self._message_handlers.add(handler)
        return lambda: self._message_handlers.discard(handler)

    def on_close(self, handler: CloseHandler) -> Callable[[], None]:
        self._close_handlers.add(handler)
        return lambda: self._close_handlers.discard(handler)

    def on_error(self, handler: ErrorHandler) -> Callable[[], None]:
        self._error_handlers.add(handler)
        return lambda: self._error_handlers.discard(handler)

    def _emit_message(self, data: str) -> None:
        for h in list(self._message_handlers):
            h(data)

    def _emit_close(self, info: CloseInfo) -> None:
        for h in list(self._close_handlers):
            h(info)

    def _emit_error(self, err: object) -> None:
        for h in list(self._error_handlers):
            h(err)


class WebSocketTransport(_HandlerMixin, Transport):
    """Default transport backed by the `websockets` library.

    Incoming frames are pumped from a background asyncio task into the registered
    message handlers. Install with ``pip install 'smooth-operator[websockets]'`` (or add
    `websockets` to your environment); if it is missing, :meth:`connect` raises.
    """

    def __init__(self, url: str, *, token: str | None = None) -> None:
        super().__init__()
        # When a connection token is supplied, fold it into the URL's ``?token=``
        # slot up front so :meth:`connect` opens the token-gated URL.
        self._url = apply_connection_token(url, token)
        self._ws: object | None = None
        self._state: TransportState = "closed"
        self._reader_task: asyncio.Task | None = None

    @property
    def state(self) -> TransportState:
        return self._state

    async def connect(self) -> None:
        try:
            import websockets  # noqa: PLC0415
        except ImportError as exc:  # pragma: no cover - exercised only without dep
            raise RuntimeError(
                "WebSocketTransport requires the `websockets` package. "
                "Install with `pip install 'smooth-operator[websockets]'`, or inject a "
                "custom Transport."
            ) from exc

        self._state = "connecting"
        self._ws = await websockets.connect(self._url)
        self._state = "open"
        self._reader_task = asyncio.create_task(self._read_loop())

    async def _read_loop(self) -> None:
        assert self._ws is not None
        try:
            async for message in self._ws:  # type: ignore[attr-defined]
                data = message if isinstance(message, str) else message.decode("utf-8")
                self._emit_message(data)
        except Exception as err:  # noqa: BLE001 - surface any read error to handlers
            self._emit_error(err)
        finally:
            self._state = "closed"
            self._emit_close({"code": None, "reason": "connection closed"})

    def send(self, data: str) -> None:
        if self._state != "open" or self._ws is None:
            raise RuntimeError(f'Cannot send: transport is "{self._state}"')
        # websockets' send() is a coroutine; schedule it without forcing callers async.
        coro = self._ws.send(data)  # type: ignore[attr-defined]
        asyncio.ensure_future(coro)

    async def close(self, code: int = 1000, reason: str = "") -> None:
        self._state = "closing"
        if self._reader_task is not None:
            self._reader_task.cancel()
        if self._ws is not None:
            await self._ws.close(code, reason)  # type: ignore[attr-defined]
        self._state = "closed"
