"""Live end-to-end test: boots the real Rust ``smooth-operator-agent-server`` and
drives real LLM turns through it over a real WebSocket using the production async
:class:`SmoothAgentClient` + :class:`WebSocketTransport`.

This is **not** a mock test. It spawns the compiled Rust binary, connects the real
``websockets``-backed transport, opens a session, sends user messages, and asserts
on the *actual* streamed tokens and the terminal ``eventual_response`` returned by
the live LLM gateway.

Gating
------
The test is skipped unless **both**:

* ``SMOOTH_AGENT_E2E == "1"`` (opt-in — keeps the default ``uv run pytest`` fast and
  offline), and
* ``SMOOAI_GATEWAY_KEY`` is set (the live gateway credential; never hardcoded).

The gateway key is read from the environment and forwarded into the spawned server's
environment. It is never printed or logged.

Running it
----------
::

    export SMOOAI_GATEWAY_KEY=$(python3 -c "import json;print(json.load(open('$HOME/.local/share/opencode/auth.json'))['smooai']['key'])")
    export SMOOTH_AGENT_E2E=1
    env -u VIRTUAL_ENV uv run pytest tests/test_e2e_live.py -v -s
"""

from __future__ import annotations

import json
import os
import socket
import subprocess
import time
import uuid
from collections.abc import Iterator
from pathlib import Path

import pytest

from smooth_operator_agent import (
    EventualResponse,
    SmoothAgentClient,
    WebSocketTransport,
)

# ── gating ──────────────────────────────────────────────────────────────────────
_E2E_ENABLED = os.getenv("SMOOTH_AGENT_E2E") == "1" and bool(os.getenv("SMOOAI_GATEWAY_KEY"))

pytestmark = pytest.mark.skipif(
    not _E2E_ENABLED,
    reason=(
        "Live E2E disabled. Set SMOOTH_AGENT_E2E=1 and SMOOAI_GATEWAY_KEY to run "
        "(see module docstring)."
    ),
)

# ── server location / config ────────────────────────────────────────────────────
_PORT = 8813
_WS_URL = f"ws://127.0.0.1:{_PORT}/ws"
_MODEL = "claude-haiku-4-5"

# The server echoes the supplied agentId back in its create-session response, and
# the wire schema types agentId as a UUID — so the "e2e" agent is identified by a
# stable, deterministic UUID derived from the label "e2e" (same id every run).
_AGENT_ID = str(uuid.uuid5(uuid.NAMESPACE_DNS, "smooth-operator-agent.e2e"))

# Candidate locations for the compiled server binary.
_BINARY_CANDIDATES = [
    Path.home() / ".cargo" / "shared-target" / "debug" / "smooth-operator-agent-server",
    # In-tree fallback if a local target/ build was produced instead.
    Path(__file__).resolve().parents[2] / "rust" / "target" / "debug" / "smooth-operator-agent-server",
]


def _resolve_binary() -> Path:
    for candidate in _BINARY_CANDIDATES:
        if candidate.is_file():
            return candidate
    pytest.fail(
        "smooth-operator-agent-server binary not found. Build it with:\n"
        "  cargo build -p smooai-smooth-operator-agent-server "
        "--bin smooth-operator-agent-server\n"
        f"(searched: {', '.join(str(p) for p in _BINARY_CANDIDATES)})"
    )


def _port_open(host: str, port: int) -> bool:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.settimeout(0.25)
        return sock.connect_ex((host, port)) == 0


def _wait_for_port(host: str, port: int, proc: subprocess.Popen, timeout: float = 30.0) -> None:
    """Block until the server is accepting connections, or fail with diagnostics."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            out = proc.stdout.read() if proc.stdout else ""
            pytest.fail(
                f"server exited early (code={proc.returncode}) before binding port "
                f"{port}.\n--- server output ---\n{out}"
            )
        if _port_open(host, port):
            return
        time.sleep(0.1)
    pytest.fail(f"server did not bind {host}:{port} within {timeout}s")


@pytest.fixture(scope="module")
def live_server() -> Iterator[str]:
    """Spawn the real Rust server with a live gateway key, yield its ws URL."""
    binary = _resolve_binary()
    key = os.environ["SMOOAI_GATEWAY_KEY"]  # presence guaranteed by the skip gate

    env = {
        **os.environ,
        "SMOOTH_AGENT_PORT": str(_PORT),
        "SMOOTH_AGENT_SEED_KB": "1",
        "SMOOTH_AGENT_MODEL": _MODEL,
        "SMOOAI_GATEWAY_KEY": key,  # forwarded, never printed
    }

    proc = subprocess.Popen(  # noqa: S603 - trusted local binary, fixed args
        [str(binary)],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
    )
    try:
        _wait_for_port("127.0.0.1", _PORT, proc)
        yield _WS_URL
    finally:
        proc.terminate()
        try:
            proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait(timeout=5)


def _reply_text(final: EventualResponse) -> str:
    """Flatten the terminal response payload to a single searchable string.

    The structured ``response`` shape is template-dependent (``responseParts`` list,
    a bare string, etc.), so we stringify the whole inner payload and search that —
    robust to template variation while still asserting against the real reply.
    """
    inner = final.data.data
    blob = inner.model_dump(by_alias=True, mode="json")
    return json.dumps(blob)


async def _run_turn(client: SmoothAgentClient, session_id: str, message: str) -> tuple[list[str], EventualResponse]:
    """Send a message, collect streamed tokens, return (tokens, terminal response)."""
    tokens: list[str] = []
    turn = client.send_message(session_id=session_id, message=message)
    async for event in turn:
        if event.type == "stream_token" and event.token:
            tokens.append(event.token)
        elif event.type == "stream_chunk":
            tokens.append(f"<chunk:{event.node}>")
    final = await turn
    return tokens, final


@pytest.mark.asyncio
async def test_live_knowledge_base_and_memory(live_server: str) -> None:
    """Real LLM turns through the live Rust service.

    1. KB lookup: the seeded "17 days" return-window fact must surface in the reply,
       and the turn must stream at least one intermediate event.
    2. Memory: a name told in one turn must be recalled in a later turn.
    """
    client = SmoothAgentClient(live_server, transport=WebSocketTransport(live_server))
    await client.connect()
    try:
        # ── session ─────────────────────────────────────────────────────────────
        session = await client.create_conversation_session(agent_id=_AGENT_ID)
        session_id = str(session.session_id)
        assert session_id, "expected a non-empty session id"
        print(f"\n[e2e] session_id={session_id}")

        # ── 1. knowledge base ────────────────────────────────────────────────────
        tokens, final = await _run_turn(
            client,
            session_id,
            "What is SmooAI's return window? Search the knowledge base.",
        )
        reply = _reply_text(final)
        print(f"[e2e] streamed {len(tokens)} events; reply={reply}")
        assert tokens, "expected at least one stream_token / stream_chunk event"
        assert "17" in reply, f"expected the seeded '17' return window in the reply, got: {reply}"

        # ── 2. memory: tell a name ───────────────────────────────────────────────
        _, _ = await _run_turn(client, session_id, "My name is Zog. Remember it.")

        # ── 3. memory: recall the name ───────────────────────────────────────────
        _, recall = await _run_turn(client, session_id, "What is my name?")
        recall_text = _reply_text(recall)
        print(f"[e2e] recall reply={recall_text}")
        assert "Zog" in recall_text, f"expected the agent to recall 'Zog', got: {recall_text}"
    finally:
        await client.disconnect()
