"""SEP extension hosting on the operator server — the live-wire integration test.

Spawns a real extension subprocess (the dependency-free Python echo peer) through
the engine :class:`ExtensionHost` and asserts the server's composition claims:

1. An extension's tools reach the turn and flow through the SAME per-agent
   ``enabled_tools`` filter the runner applies (so an allow-list drops an extension
   tool exactly like a built-in) — the Python sibling of the Rust
   ``sep_extension_host.rs``.
2. Driven end-to-end through a real ``send_message`` turn, the model can call the
   extension's tool and its result streams back.
3. Trust is default-deny: with ``SMOOTH_EXTENSIONS_ALLOW`` unset, no host is built.

Plus the ``ui/confirm`` -> confirmation-frame bridge unit tests (no subprocess).
"""

from __future__ import annotations

import asyncio
import sys
from pathlib import Path

from smooth_operator_core import MockLlmProvider

from smooth_operator_server.agent_config import AgentConfig, EnabledTool, StaticAgentConfigResolver, filter_tools
from smooth_operator_server.confirmation import ConfirmationRegistry
from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.extensions import ConfirmUiProvider, build_extension_host, parse_allowlist
from smooth_operator_server.session_store import InMemorySessionStore

ECHO_PEER = Path(__file__).parent / "sep" / "echo_peer.py"


def _write_echo_manifest(root: Path) -> Path:
    ext_dir = root / "echo"
    ext_dir.mkdir(parents=True, exist_ok=True)
    (ext_dir / "extension.toml").write_text(
        f'name = "echo"\nversion = "0.1.0"\n[run]\ncommand = "{sys.executable}"\nargs = ["{ECHO_PEER}"]\n'
        "[capabilities]\ntools = true\n"
    )
    return root


def _tool_results(events: list[dict]) -> list[str]:
    out = []
    for e in events:
        state = e.get("data", {}).get("state") if isinstance(e, dict) else None
        result = (state or {}).get("rawResponse", {}).get("toolResult") if isinstance(state, dict) else None
        if result:
            out.append(result["result"])
    return out


# ---- trust: default deny ----


def test_parse_allowlist_denies_by_default() -> None:
    assert parse_allowlist(None) == []
    assert parse_allowlist("") == []
    assert parse_allowlist("  , ,") == []
    assert parse_allowlist("todo") == ["todo"]
    assert parse_allowlist(" todo , gate ") == ["todo", "gate"]


async def test_build_host_is_none_when_allowlist_empty(monkeypatch, tmp_path) -> None:
    monkeypatch.delenv("SMOOTH_EXTENSIONS_ALLOW", raising=False)
    monkeypatch.setenv("SMOOTH_EXTENSIONS_DIR", str(_write_echo_manifest(tmp_path)))
    turn = await build_extension_host("sess-1", "req-1", lambda _e: None, ConfirmationRegistry())
    assert turn is None  # default deny — nothing spawned


# ---- host + enabled_tools filtering parity (mirrors the Rust test) ----


async def test_extension_tool_reaches_host_and_honors_enabled_tools(monkeypatch, tmp_path) -> None:
    monkeypatch.setenv("SMOOTH_EXTENSIONS_ALLOW", "echo")
    monkeypatch.setenv("SMOOTH_EXTENSIONS_DIR", str(_write_echo_manifest(tmp_path)))
    turn = await build_extension_host("sess-1", "req-1", lambda _e: None, ConfirmationRegistry())
    assert turn is not None
    try:
        assert turn.host.names() == ["echo"]
        tools = turn.host.tools()
        assert any(t.name == "echo.say" for t in tools), [t.name for t in tools]

        # enabled_tools that NAMES the ext tool keeps it; one that excludes it drops it.
        keep = AgentConfig(enabled_tools=[EnabledTool("echo.say")])
        drop = AgentConfig(enabled_tools=[EnabledTool("some_builtin")])
        assert any(t.name == "echo.say" for t in filter_tools(turn.host.tools(), keep))
        assert not any(t.name == "echo.say" for t in filter_tools(turn.host.tools(), drop))

        # The proxy executes end-to-end over tool/execute.
        say = next(t for t in turn.host.tools() if t.name == "echo.say")
        assert await say.execute({"phrase": "hi there"}) == "hi there"
    finally:
        await turn.teardown()


# ---- end-to-end through a real send_message turn ----


async def test_echo_extension_tool_runs_through_a_real_turn(monkeypatch, tmp_path) -> None:
    monkeypatch.setenv("SMOOTH_EXTENSIONS_ALLOW", "echo")
    monkeypatch.setenv("SMOOTH_EXTENSIONS_DIR", str(_write_echo_manifest(tmp_path)))

    store = InMemorySessionStore()
    session = await store.create_session("agent-x", None, None)

    mock = MockLlmProvider()
    mock.push_tool_call("call-1", "echo.say", '{"phrase": "hello"}')  # LLM asks for the ext tool
    mock.push_text("all done")  # follow-up reply after the tool result

    # The agent enables the extension tool by name (SMOODEV-590 tool_config).
    config = AgentConfig(enabled_tools=[EnabledTool("echo.say")])
    dispatcher = FrameDispatcher(
        store,
        mock,
        tools=[],
        agent_config_resolver=StaticAgentConfigResolver({"agent-x": config}),
    )

    events: list[dict] = []
    await dispatcher.dispatch(
        '{"action":"send_message","sessionId":"%s","message":"go"}' % session.session_id,
        events.append,
    )
    await dispatcher.wait_for_turns()

    # The echo extension's tool ran and echoed the phrase straight back.
    assert "hello" in _tool_results(events)


async def test_extension_tool_filtered_out_when_not_enabled(monkeypatch, tmp_path) -> None:
    # enabled_tools that EXCLUDES echo.say means the model never sees it → the tool
    # call resolves to "unknown tool" instead of running the extension.
    monkeypatch.setenv("SMOOTH_EXTENSIONS_ALLOW", "echo")
    monkeypatch.setenv("SMOOTH_EXTENSIONS_DIR", str(_write_echo_manifest(tmp_path)))

    store = InMemorySessionStore()
    session = await store.create_session("agent-x", None, None)
    mock = MockLlmProvider()
    mock.push_tool_call("call-1", "echo.say", '{"phrase": "hello"}')
    mock.push_text("done")

    config = AgentConfig(enabled_tools=[EnabledTool("some_other_tool")])  # excludes echo.say
    dispatcher = FrameDispatcher(
        store, mock, tools=[], agent_config_resolver=StaticAgentConfigResolver({"agent-x": config})
    )
    events: list[dict] = []
    await dispatcher.dispatch(
        '{"action":"send_message","sessionId":"%s","message":"go"}' % session.session_id, events.append
    )
    await dispatcher.wait_for_turns()

    results = _tool_results(events)
    assert not any(r == "hello" for r in results)
    assert any("unknown tool" in r for r in results)


# ---- ui/confirm -> confirmation-frame bridge (no subprocess) ----


def _provider(sink, registry, session="sess-1"):
    return ConfirmUiProvider(sink, "req-1", session, registry)


async def test_confirm_emits_frame_and_resolves_on_approval() -> None:
    frames: list[dict] = []
    registry = ConfirmationRegistry()
    provider = _provider(frames.append, registry)

    fut = asyncio.ensure_future(provider.ui_request("todo", {"kind": "confirm", "prompt": "Delete file?"}))
    await asyncio.sleep(0)  # let the provider emit the frame + register

    assert frames[0]["type"] == "write_confirmation_required"
    assert frames[0]["data"]["data"]["toolId"] == "todo"
    assert frames[0]["data"]["data"]["actionDescription"] == "Delete file?"

    assert registry.resolve("sess-1", True)
    assert await fut == {"confirmed": True}


async def test_confirm_resolves_false_on_denial() -> None:
    frames: list[dict] = []
    registry = ConfirmationRegistry()
    provider = _provider(frames.append, registry)
    fut = asyncio.ensure_future(provider.ui_request("gate", {"kind": "confirm", "prompt": "Proceed?"}))
    await asyncio.sleep(0)
    registry.resolve("sess-1", False)
    assert await fut == {"confirmed": False}


async def test_render_only_kinds_accept_and_drop() -> None:
    provider = _provider(lambda _e: None, ConfirmationRegistry())
    for kind in ("notify", "set_status", "set_widget", "set_title"):
        assert await provider.ui_request("x", {"kind": kind, "message": "hi"}) == {}


async def test_unsupported_interactive_kinds_cancel() -> None:
    provider = _provider(lambda _e: None, ConfirmationRegistry())
    for kind in ("select", "input"):
        assert await provider.ui_request("x", {"kind": kind, "prompt": "?"}) == {"cancelled": True}
