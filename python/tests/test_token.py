"""Connection-token option (``?token=`` for token-gated, local-flavor servers).

The server reads the connection token from the ``?token=`` query slot of the WS
URL. These tests pin the client/transport contract: a supplied ``token`` lands in
the opened URL's query, an existing query is preserved (merged, not clobbered),
and the default (no-token) path is byte-for-byte unchanged.
"""

from __future__ import annotations

from urllib.parse import parse_qs, urlsplit

from smooth_operator import SmoothAgentClient, WebSocketTransport
from smooth_operator.transport import apply_connection_token


def _query(url: str) -> dict[str, list[str]]:
    return parse_qs(urlsplit(url).query, keep_blank_values=True)


def test_transport_token_lands_in_connection_url_query() -> None:
    t = WebSocketTransport("wss://local.example/ws", token="secret123")
    assert _query(t._url)["token"] == ["secret123"]


def test_transport_token_preserves_existing_query() -> None:
    t = WebSocketTransport("wss://local.example/ws?foo=bar", token="secret123")
    q = _query(t._url)
    assert q["foo"] == ["bar"]
    assert q["token"] == ["secret123"]


def test_client_default_transport_carries_token() -> None:
    client = SmoothAgentClient("wss://local.example/ws?foo=bar", token="secret123")
    transport = client._transport
    assert isinstance(transport, WebSocketTransport)
    q = _query(transport._url)
    assert q["token"] == ["secret123"]
    assert q["foo"] == ["bar"]


def test_no_token_leaves_url_unchanged() -> None:
    url = "wss://local.example/ws?foo=bar"
    assert WebSocketTransport(url)._url == url
    assert apply_connection_token(url, None) == url


def test_token_is_url_encoded() -> None:
    # A token with reserved characters must be percent-encoded, then decode back
    # to the original value on the server side.
    token = "a b&c=d/e"
    out = apply_connection_token("wss://local.example/ws", token)
    assert "token=a+b%26c%3Dd%2Fe" in out or "token=a%20b%26c%3Dd%2Fe" in out
    assert _query(out)["token"] == [token]


def test_token_replaces_existing_token_param() -> None:
    out = apply_connection_token("wss://local.example/ws?token=old", "new")
    assert _query(out)["token"] == ["new"]
