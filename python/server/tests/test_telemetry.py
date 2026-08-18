"""Telemetry coverage for the production turn path (:meth:`TurnRunner.run`).

The Python sibling of the Rust ``tests/telemetry.rs``: drives a real streaming turn
on ``MockLlmProvider`` (a scripted ``knowledge_search`` tool call, then the final
answer) through an in-memory span exporter — no live OTLP collector — and asserts it
emits:

1. A ``gen_ai.chat`` turn span carrying ``gen_ai.system``, ``gen_ai.request.model``,
   ``gen_ai.conversation.id``, ``gen_ai.agent.name``, and ``smooai.org_id``.
2. A child ``gen_ai.tool`` span carrying ``gen_ai.tool.name`` and the (redacted)
   ``gen_ai.tool.call.arguments`` the model passed.
"""

from __future__ import annotations

import pytest
from opentelemetry import trace
from opentelemetry.sdk.trace import TracerProvider
from opentelemetry.sdk.trace.export import SimpleSpanProcessor
from opentelemetry.sdk.trace.export.in_memory_span_exporter import InMemorySpanExporter
from smooth_operator_core import MockLlmProvider

from smooth_operator_server import telemetry
from smooth_operator_server.session_store import InMemorySessionStore
from smooth_operator_server.turn_runner import TurnRunner


class _KnowledgeSearchTool:
    """Duck-typed engine Tool the scripted call resolves to."""

    name = "knowledge_search"
    description = "search the knowledge base"
    parameters = {"type": "object", "properties": {"query": {"type": "string"}}}

    async def execute(self, arguments: dict) -> str:
        return "Items are accepted within 30 days for a full refund."


@pytest.fixture(scope="module")
def span_exporter() -> InMemorySpanExporter:
    """Install a global TracerProvider backed by an in-memory exporter once. The
    global provider can only be set for the process, so this is module-scoped; the
    exporter is cleared per test."""
    exporter = InMemorySpanExporter()
    provider = TracerProvider()
    provider.add_span_processor(SimpleSpanProcessor(exporter))
    # Set only if the process hasn't already installed a real provider (it hasn't in
    # tests — OTEL_EXPORTER_OTLP_ENDPOINT is unset). SimpleSpanProcessor flushes on
    # span end so finished spans are visible immediately.
    trace.set_tracer_provider(provider)
    return exporter


@pytest.mark.asyncio
async def test_turn_emits_gen_ai_spans_with_org_and_tool_args(span_exporter: InMemorySpanExporter) -> None:
    span_exporter.clear()

    mock = MockLlmProvider()
    mock.push_tool_call("call_kb_1", "knowledge_search", '{"query": "return policy refund window"}')
    mock.push_text("Items are accepted within 30 days for a full refund.")

    runner = TurnRunner(
        chat_client=mock,
        store=InMemorySessionStore(),
        model="openai/gpt-4o",
        tools=[_KnowledgeSearchTool()],
        org_id="org-telemetry",
    )
    await runner.run(
        conversation_id="conv-otel-srv",
        request_id="req-otel-srv",
        user_message="what is the return policy?",
        sink=lambda _event: None,
    )

    spans = {s.name: s for s in span_exporter.get_finished_spans()}

    # (1) The turn span carries system, model, conversation, agent, and org.
    assert telemetry.SPAN_CHAT in spans, f"expected a gen_ai.chat span; got {list(spans)}"
    chat = spans[telemetry.SPAN_CHAT]
    assert chat.attributes[telemetry.GEN_AI_SYSTEM] == telemetry.SYSTEM_NAME
    assert chat.attributes[telemetry.GEN_AI_REQUEST_MODEL] == "openai/gpt-4o"
    assert chat.attributes[telemetry.GEN_AI_CONVERSATION_ID] == "conv-otel-srv"
    assert chat.attributes[telemetry.GEN_AI_AGENT_NAME] == telemetry.AGENT_NAME
    assert chat.attributes[telemetry.SMOOAI_ORG_ID] == "org-telemetry"

    # (2) A child tool span with the tool name + arguments, parented to the turn span.
    assert telemetry.SPAN_TOOL in spans, f"expected a gen_ai.tool span; got {list(spans)}"
    tool = spans[telemetry.SPAN_TOOL]
    assert tool.attributes[telemetry.GEN_AI_TOOL_NAME] == "knowledge_search"
    args = tool.attributes[telemetry.GEN_AI_TOOL_ARGUMENTS]
    assert "return policy refund window" in args, f"tool args should carry the query; got {args!r}"
    assert tool.parent is not None and tool.parent.span_id == chat.context.span_id

    # Being a child is NOT enough. The OTLP ingest builds a span's attributes from the
    # resource attrs plus THAT span's own, with no parent inheritance, so the tool span
    # repeats the identifiers itself — and without gen_ai.system it fails the ingest's
    # LLM-event gate outright and is discarded, which is what happened to Rust's tool
    # spans for their entire existence (zero rows with operation_name='tool', all time).
    assert tool.attributes[telemetry.GEN_AI_SYSTEM] == telemetry.SYSTEM_NAME
    assert tool.attributes[telemetry.GEN_AI_OPERATION_NAME] == telemetry.OPERATION_TOOL
    assert tool.attributes[telemetry.GEN_AI_CONVERSATION_ID] == "conv-otel-srv"
    assert tool.attributes[telemetry.SMOOAI_ORG_ID] == "org-telemetry"

    # Must be exactly "chat"/"tool" — the ingest takes the attribute verbatim when
    # present and its queries filter on operation_name = 'tool'.
    assert chat.attributes[telemetry.GEN_AI_OPERATION_NAME] == telemetry.OPERATION_CHAT

    # Cost: exactly one of the two is ever set, and a zero is never exported as a real
    # cost (it means "unpriced", not "free").
    if telemetry.GEN_AI_USAGE_COST_USD in chat.attributes:
        assert chat.attributes[telemetry.GEN_AI_USAGE_COST_USD] > 0
        assert telemetry.COST_UNAVAILABLE not in chat.attributes
    else:
        assert chat.attributes[telemetry.COST_UNAVAILABLE] == telemetry.COST_UNAVAILABLE_UNPRICED


def test_redact_tool_arguments_scrubs_secret_named_keys() -> None:
    out = telemetry.redact_tool_arguments('{"query":"weather","api_key":"sk-live-123"}')
    assert '"query":"weather"' in out
    assert "sk-live-123" not in out
    assert "[REDACTED]" in out
    # Non-JSON passes through; length is capped.
    assert telemetry.redact_tool_arguments("not json") == "not json"
    long = "x" * (telemetry.MAX_TOOL_ARGS_LEN + 50)
    assert len(telemetry.redact_tool_arguments(long)) <= telemetry.MAX_TOOL_ARGS_LEN + 1
