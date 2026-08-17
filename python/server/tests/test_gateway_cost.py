"""The gateway's per-request cost reaches the turn's ``usage`` (and so
``eventual_response.usage.costUsd``).

Cost is reported ONLY in a response header. The server used to inject the raw
``AsyncOpenAI`` SDK, whose parsed response drops headers — so core's cost-header
parser had nothing to read and every turn reported ``costUsd: 0``. These run a REAL
turn against a REAL local gateway (``http.server`` speaking SSE), so they fail if
the server ever goes back to injecting a header-dropping client.
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer
from typing import Any

import pytest
from smooth_operator_core import GatewayLlmProvider

from smooth_operator_server.server import _build_gateway_client
from smooth_operator_server.session_store import InMemorySessionStore
from smooth_operator_server.turn_runner import TurnRunner


class _Gateway:
    """A local OpenAI-compatible endpoint that streams one SSE reply."""

    def __init__(self, headers: dict[str, str]) -> None:
        script_headers = headers

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args: Any) -> None:  # keep pytest output clean
                pass

            def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler's naming
                self.rfile.read(int(self.headers["Content-Length"]))
                self.send_response(200)
                for name, value in script_headers.items():
                    self.send_header(name, value)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                self._sse({"choices": [{"index": 0, "delta": {"content": "Seventeen days."}}]})
                self._sse({"choices": [], "usage": {"prompt_tokens": 10, "completion_tokens": 5}})
                self.wfile.write(b"data: [DONE]\n\n")

            def _sse(self, payload: dict[str, Any]) -> None:
                self.wfile.write(f"data: {json.dumps(payload)}\n\n".encode())

        self._server = HTTPServer(("127.0.0.1", 0), Handler)
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)

    def __enter__(self) -> "_Gateway":
        self._thread.start()
        return self

    def __exit__(self, *_exc: Any) -> None:
        self._server.shutdown()
        self._server.server_close()
        self._thread.join(timeout=5)

    @property
    def base_url(self) -> str:
        host, port = self._server.server_address[:2]
        return f"http://{host}:{port}/v1"


async def _turn_usage(headers: dict[str, str]) -> dict[str, Any] | None:
    """Run one real turn through the server's TurnRunner; return its usage dict."""
    with _Gateway(headers) as gw:
        runner = TurnRunner(
            chat_client=GatewayLlmProvider(base_url=gw.base_url, api_key="k"),
            store=InMemorySessionStore(),
        )
        result = await runner.run(
            conversation_id="conv-1",
            request_id="r-1",
            user_message="how long can I return?",
            sink=lambda _event: None,
        )
    return result.usage


@pytest.mark.asyncio
async def test_header_cost_reaches_the_turn_usage() -> None:
    usage = await _turn_usage({"x-litellm-response-cost-margin-amount": "0.25"})

    assert usage is not None
    assert usage["costUsd"] == 0.25
    # Token counts still come from the stream's usage chunk, unaffected.
    assert usage["promptTokens"] == 10
    assert usage["completionTokens"] == 5


@pytest.mark.asyncio
async def test_zero_margin_does_not_zero_real_spend() -> None:
    usage = await _turn_usage({"x-litellm-response-cost-margin-amount": "0", "x-litellm-response-cost-original": "0.5"})

    assert usage is not None
    assert usage["costUsd"] == 0.5


@pytest.mark.asyncio
async def test_absent_and_all_zero_headers_are_both_unmeasured() -> None:
    """Absent and present-but-zero must be INDISTINGUISHABLE, and neither may be
    taken at face value as a real $0 — both fall through to the local pricing
    estimate. (The default model is priced, so that estimate is non-zero here; the
    invariant is the equality and the fall-through, not the specific number.)"""
    absent = (await _turn_usage({}))["costUsd"]
    all_zero = (await _turn_usage({"x-litellm-response-cost": "0", "x-cost-usd": "0"}))["costUsd"]

    assert absent == all_zero
    # Fell back to the local estimate rather than locking in the gateway's zero.
    assert absent > 0
    # And emphatically not a value the gateway supplied in the other tests.
    assert absent not in (0.25, 0.5)


def test_build_gateway_client_wires_a_header_reading_provider(monkeypatch) -> None:
    # The tests above inject the provider directly, so they pin the server PIPELINE.
    # This pins the WIRING — that the server hands the engine a client which can see the
    # cost header at all. Injecting the raw AsyncOpenAI (the bug) fails here, nowhere else.
    monkeypatch.setenv("SMOOAI_GATEWAY_KEY", "k")
    monkeypatch.setenv("SMOOAI_GATEWAY_URL", "http://127.0.0.1:1/v1")

    assert isinstance(_build_gateway_client(), GatewayLlmProvider)


def test_no_key_still_serves_protocol_only(monkeypatch) -> None:
    monkeypatch.delenv("SMOOAI_GATEWAY_KEY", raising=False)

    assert _build_gateway_client() is None
