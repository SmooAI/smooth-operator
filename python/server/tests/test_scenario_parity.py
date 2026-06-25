"""Scenario parity runner — the Python reference implementation.

Runs every scenario in ``spec/conformance/scenarios/*.json`` through the Python
server and asserts the normalized protocol output matches. This is the corpus that
holds the five native servers to parity: port this ~one-file runner into the TS / Go
/ C# / Rust server suites and, when all five run the same corpus green, the servers
are at protocol parity.

The turn is deterministic because the engine runs on the same ``MockLlmProvider``
script the scenario declares — no gateway, no flakiness.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest
import websockets
from smooth_operator_core import FunctionTool, MockLlmProvider

from smooth_operator_server import ServerState, serve
from smooth_operator_server.session_store import InMemorySessionStore

SCENARIOS_DIR = Path(__file__).resolve().parents[3] / "spec" / "conformance" / "scenarios"
SCENARIOS = sorted(SCENARIOS_DIR.glob("*.json"))


def _dot(obj: dict, path: str):
    """Resolve a dotted path (``data.data.response.responseParts``) into a nested dict."""
    cur = obj
    for part in path.split("."):
        cur = cur[part]
    return cur


def _build_mock(script: list[dict]) -> MockLlmProvider:
    mock = MockLlmProvider()
    for entry in script:
        if entry["kind"] == "text":
            mock.push_text(entry["text"])
        elif entry["kind"] == "toolCall":
            mock.push_tool_call(entry.get("id", "call-1"), entry["name"], entry["arguments"])
        else:  # pragma: no cover - guards a malformed corpus
            raise ValueError(f"unknown mockLlmScript kind: {entry['kind']!r}")
    return mock


def _build_tools(specs: list[dict]) -> list[FunctionTool]:
    """Build deterministic test tools from a scenario's ``server.tools`` directive.
    Each tool ignores its arguments and returns the spec's fixed ``result`` string,
    so a tool-call turn is fully deterministic across every server."""
    tools: list[FunctionTool] = []
    for spec in specs:
        result = spec["result"]

        async def _fn(_args: dict, _result: str = result) -> str:
            return _result

        tools.append(
            FunctionTool(
                name=spec["name"],
                description=spec.get("description", ""),
                parameters=spec.get("parameters", {"type": "object", "properties": {}}),
                func=_fn,
            )
        )
    return tools


def _subst(value, vars_: dict):
    """Replace ``{{name}}`` placeholders in string fields from captured vars."""
    if isinstance(value, str) and value.startswith("{{") and value.endswith("}}"):
        return vars_[value[2:-2]]
    if isinstance(value, dict):
        return {k: _subst(v, vars_) for k, v in value.items()}
    return value


@pytest.mark.parametrize("path", SCENARIOS, ids=[p.stem for p in SCENARIOS])
@pytest.mark.asyncio
async def test_scenario_parity(path: Path) -> None:
    scenario = json.loads(path.read_text())
    mock = _build_mock(scenario.get("mockLlmScript", []))
    server_spec = scenario.get("server", {})
    tools = _build_tools(server_spec.get("tools", []))
    # `server.confirmTools` gates tools behind write-confirmation HITL: a turn that
    # calls one parks and emits `write_confirmation_required` until the client sends
    # `confirm_tool_action`. Empty/absent → no gating (every existing scenario).
    confirm_tools = server_spec.get("confirmTools", [])
    server, _ = await _start(chat_client=mock, tools=tools, confirm_tools=confirm_tools)
    vars_: dict = {}
    try:
        async with websockets.connect(server.ws_url()) as ws:
            for step in scenario["steps"]:
                await ws.send(json.dumps(_subst(step["send"], vars_)))
                await _match_expected(ws, step["expect"], vars_)
    finally:
        await server.shutdown()


async def _start(chat_client=None, tools=None, confirm_tools=None):
    state = ServerState(
        store=InMemorySessionStore(),
        chat_client=chat_client,
        tools=tools or [],
        confirm_tools=confirm_tools or [],
    )
    server = await serve(state, "127.0.0.1", 0)
    return server, state


async def _next_event(ws) -> dict:
    """Next protocol event, skipping non-semantic keepalive/pong frames."""
    while True:
        event = json.loads(await ws.recv())
        if event.get("type") not in ("keepalive", "pong"):
            return event


async def _match_expected(ws, matchers: list[dict], vars_: dict) -> None:
    """Match the outbound event stream against an ordered list of matchers."""
    pending = None  # one-event lookahead when a `repeat` matcher overruns
    for m in matchers:
        accumulated = ""
        while True:
            event = pending or await _next_event(ws)
            pending = None
            if m.get("repeat") and event.get("type") != m["type"]:
                # the repeated run ended; this event belongs to the next matcher
                pending = event
                break
            assert event["type"] == m["type"], f"expected {m['type']}, got {event['type']}"
            if "status" in m:
                assert event["status"] == m["status"], f"{m['type']}: status {event['status']} != {m['status']}"
            if "statusGte" in m:
                assert event["status"] >= m["statusGte"], f"{m['type']}: status {event['status']} < {m['statusGte']}"
            for path, expected in m.get("assert", {}).items():
                assert _dot(event, path) == expected, f"{m['type']}: {path} = {_dot(event, path)!r} != {expected!r}"
            for var, path in m.get("capture", {}).items():
                vars_[var] = _dot(event, path)
            if "accumulate" in m:
                accumulated += event[m["accumulate"]]
            if not m.get("repeat"):
                break
        if "assertAccumulated" in m:
            assert accumulated == m["assertAccumulated"], f"accumulated {accumulated!r} != {m['assertAccumulated']!r}"
