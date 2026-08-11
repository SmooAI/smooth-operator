"""File-transfer parity (spec PR #342): ``send_message.images[]`` reach the model as
OpenAI ``image_url`` content parts, ``send_message.files[]`` are surfaced on the
per-turn context (never sent to the model), and a host tool's ``send_file`` directive
is drained onto ``eventual_response.directive``. Mirrors the Rust reference server.
"""

from __future__ import annotations

import json

import pytest
from smooth_operator_core import FunctionTool, MockLlmProvider

from smooth_operator_server import protocol
from smooth_operator_server.agent_config import StaticAgentConfigResolver
from smooth_operator_server.dispatcher import FrameDispatcher, _parse_attachments
from smooth_operator_server.session_store import InMemorySessionStore
from smooth_operator_server.turn_runner import TurnContext, TurnRunner, _build_user_content

# ── _build_user_content (image → OpenAI content parts) ───────────────────────


def test_build_user_content_maps_images() -> None:
    parts = _build_user_content(
        "look at this",
        [
            {"url": "data:image/png;base64,AAA", "detail": "high"},
            {"url": "https://x/y.png"},  # no detail → omitted
        ],
    )
    assert parts[0] == {"type": "text", "text": "look at this"}
    assert parts[1] == {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA", "detail": "high"}}
    assert parts[2] == {"type": "image_url", "image_url": {"url": "https://x/y.png"}}


def test_build_user_content_is_fail_soft() -> None:
    """Malformed entries (no/blank url, non-string detail, non-dict) are dropped; a
    leading text part is always present."""
    parts = _build_user_content(
        "hi",
        [
            {"detail": "high"},  # no url → dropped
            {"url": ""},  # blank url → dropped
            "nonsense",  # not a dict → dropped
            {"url": "https://ok/1.png", "detail": 5},  # bad detail → kept, detail dropped
        ],
    )
    assert parts == [
        {"type": "text", "text": "hi"},
        {"type": "image_url", "image_url": {"url": "https://ok/1.png"}},
    ]


# ── _parse_attachments (fail-soft images/files parsing) ──────────────────────


def test_parse_attachments_images() -> None:
    parsed = _parse_attachments(
        [{"url": "https://a/1.png", "detail": "low"}, {"detail": "high"}, "junk", 7],
        ("url",),
    )
    assert parsed == [{"url": "https://a/1.png", "detail": "low"}]


def test_parse_attachments_files_require_name_and_url() -> None:
    parsed = _parse_attachments(
        [
            {"name": "r.csv", "url": "data:text/csv;base64,AA", "mimeType": "text/csv"},
            {"name": "no-url.csv"},  # missing url → dropped
            {"url": "data:...;base64,BB"},  # missing name → dropped
        ],
        ("name", "url"),
    )
    assert parsed == [{"name": "r.csv", "url": "data:text/csv;base64,AA", "mimeType": "text/csv"}]


def test_parse_attachments_non_list_is_empty() -> None:
    assert _parse_attachments(None, ("url",)) == []
    assert _parse_attachments("garbage", ("url",)) == []


# ── images attach to the model's user message (integration) ──────────────────


@pytest.mark.asyncio
async def test_images_attach_to_model_message() -> None:
    mock = MockLlmProvider()
    mock.push_text("I see it.")
    runner = TurnRunner(chat_client=mock, store=InMemorySessionStore())
    ctx = TurnContext(images=[{"url": "data:image/png;base64,AAA", "detail": "auto"}])

    await runner.run("conv-img", "r-1", "what is this", sink=lambda _e: None, context=ctx)

    content = mock.calls[0].messages[-1]["content"]
    assert isinstance(content, list), "images present ⇒ user message content is a parts list"
    assert content[0] == {"type": "text", "text": "what is this"}
    assert {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA", "detail": "auto"}} in content


@pytest.mark.asyncio
async def test_no_images_keeps_plain_string_message() -> None:
    """Back-compat: a text-only turn sends the bare string, byte-identical to before."""
    mock = MockLlmProvider()
    mock.push_text("hi")
    runner = TurnRunner(chat_client=mock, store=InMemorySessionStore())

    await runner.run("conv-txt", "r-1", "hello", sink=lambda _e: None, context=TurnContext())

    assert mock.calls[0].messages[-1]["content"] == "hello"


# ── files ride the context, never the model ──────────────────────────────────


@pytest.mark.asyncio
async def test_files_stay_off_the_model_message() -> None:
    mock = MockLlmProvider()
    mock.push_text("noted")
    runner = TurnRunner(chat_client=mock, store=InMemorySessionStore())
    ctx = TurnContext(files=[{"name": "data.csv", "url": "data:text/csv;base64,Zm9v"}])

    await runner.run("conv-file", "r-1", "process the file", sink=lambda _e: None, context=ctx)

    # File bytes/urls must NOT leak into what the model saw.
    blob = json.dumps(mock.calls[0].messages)
    assert "Zm9v" not in blob and "data.csv" not in blob
    # But they remain available to host tools on the context.
    assert ctx.files == [{"name": "data.csv", "url": "data:text/csv;base64,Zm9v"}]


# ── tool → directive sink → eventual_response ────────────────────────────────


def _send_file_tool(ctx: TurnContext, directive: dict) -> FunctionTool:
    async def _cb(_args: dict) -> str:
        ctx.set_directive(directive)
        return "sent"

    return FunctionTool("send_file", "deliver a file to the user", {"type": "object", "properties": {}}, _cb)


@pytest.mark.asyncio
async def test_tool_directive_drains_onto_turn_result() -> None:
    ctx = TurnContext()
    directive = {"type": "send_file", "files": [{"name": "r.csv", "url": "data:text/csv;base64,AA"}]}
    mock = MockLlmProvider()
    mock.push_tool_call("c1", "send_file", "{}")
    mock.push_text("Here is your file.")
    runner = TurnRunner(chat_client=mock, store=InMemorySessionStore(), tools=[_send_file_tool(ctx, directive)])

    result = await runner.run("conv-dir", "r-1", "send me the report", sink=lambda _e: None, context=ctx)

    assert result.directive == directive


@pytest.mark.asyncio
async def test_directive_last_write_wins() -> None:
    ctx = TurnContext()
    ctx.set_directive({"type": "send_file", "files": [{"name": "a", "url": "u1"}]})
    ctx.set_directive({"type": "send_file", "files": [{"name": "b", "url": "u2"}]})
    assert ctx.directive == {"type": "send_file", "files": [{"name": "b", "url": "u2"}]}


def test_eventual_response_omits_directive_when_none() -> None:
    ev = protocol.eventual_response("r-1", 200, "m-1", {"text": "hi"}, needs_escalation=False, citations=None)
    assert "directive" not in ev["data"]["data"]


def test_eventual_response_attaches_directive_when_present() -> None:
    directive = {"type": "send_file", "files": [{"name": "r.csv", "url": "data:text/csv;base64,AA"}]}
    ev = protocol.eventual_response(
        "r-1", 200, "m-1", {"text": "hi"}, needs_escalation=False, citations=None, directive=directive
    )
    assert ev["data"]["data"]["directive"] == directive


# ── end-to-end through the dispatcher ────────────────────────────────────────


@pytest.mark.asyncio
async def test_dispatcher_forwards_turn_directive_to_wire(monkeypatch: pytest.MonkeyPatch) -> None:
    """A directive a turn produces reaches ``eventual_response.directive``. The Python
    server has no per-turn tool-provider seam yet (static tools can't reach the
    dispatcher's per-turn ``TurnContext``), so drive the dispatcher's forwarding line
    directly: stub the runner to return a directive-bearing ``TurnResult``."""
    import smooth_operator_server.dispatcher as disp
    from smooth_operator_server.turn_runner import TurnResult

    store = InMemorySessionStore()
    session = await store.create_session("agent-x", None, None)
    directive = {"type": "send_file", "files": [{"name": "r.csv", "url": "data:text/csv;base64,AA"}]}

    class _StubRunner:
        def __init__(self, *_a, **_k) -> None:
            pass

        async def run(self, conversation_id, request_id, message, sink, session_id=None, context=None):
            return TurnResult(reply="Delivered.", message_id="m-1", directive=directive)

    monkeypatch.setattr(disp, "TurnRunner", _StubRunner)

    events: list[dict] = []
    dispatcher = FrameDispatcher(
        store, MockLlmProvider(), tools=[], agent_config_resolver=StaticAgentConfigResolver({})
    )
    await dispatcher.dispatch(
        json.dumps({"action": "send_message", "sessionId": session.session_id, "message": "send me the report"}),
        events.append,
    )
    await dispatcher.wait_for_turns()

    eventual = next(e for e in events if e["type"] == "eventual_response")
    assert eventual["data"]["data"]["directive"] == directive


@pytest.mark.asyncio
async def test_dispatcher_parses_images_and_files_onto_context(monkeypatch: pytest.MonkeyPatch) -> None:
    """The dispatcher parses ``images``/``files`` off the frame (fail-soft) onto the
    per-turn ``TurnContext`` it hands the runner."""
    import smooth_operator_server.dispatcher as disp
    from smooth_operator_server.turn_runner import TurnResult

    store = InMemorySessionStore()
    session = await store.create_session("agent-x", None, None)
    seen: dict = {}

    class _CaptureRunner:
        def __init__(self, *_a, **_k) -> None:
            pass

        async def run(self, conversation_id, request_id, message, sink, session_id=None, context=None):
            seen["context"] = context
            return TurnResult(reply="ok", message_id="m-1")

    monkeypatch.setattr(disp, "TurnRunner", _CaptureRunner)

    dispatcher = FrameDispatcher(
        store, MockLlmProvider(), tools=[], agent_config_resolver=StaticAgentConfigResolver({})
    )
    await dispatcher.dispatch(
        json.dumps(
            {
                "action": "send_message",
                "sessionId": session.session_id,
                "message": "hi",
                "images": [{"url": "https://a/1.png"}, {"detail": "high"}],  # 2nd dropped
                "files": [{"name": "r.csv", "url": "data:text/csv;base64,AA"}, {"name": "bad"}],  # 2nd dropped
            }
        ),
        lambda _e: None,
    )
    await dispatcher.wait_for_turns()

    ctx: TurnContext = seen["context"]
    assert ctx.images == [{"url": "https://a/1.png"}]
    assert ctx.files == [{"name": "r.csv", "url": "data:text/csv;base64,AA"}]
