"""Unit tests for the TurnRunner's auto-context citation build.

A grounded turn must carry the knowledge hits that grounded the answer in
``TurnResult.citations`` (mirroring the TS/C#/Rust servers); a turn with no
knowledge base must carry none — the eventual_response then omits the array.
"""

from __future__ import annotations

import pytest
from smooth_operator_core import InMemoryKnowledge, MockLlmProvider

from smooth_operator_server.session_store import InMemorySessionStore
from smooth_operator_server.turn_runner import (
    CITATION_SNIPPET_MAX_CHARS,
    TurnRunner,
)


def _mock(text: str) -> MockLlmProvider:
    mock = MockLlmProvider()
    mock.push_text(text)
    return mock


async def _run(knowledge, user_message: str = "what is the return policy?"):
    runner = TurnRunner(
        chat_client=_mock("Our return window is 30 days."),
        store=InMemorySessionStore(),
        knowledge=knowledge,
    )
    return await runner.run(
        conversation_id="conv-1",
        request_id="r-1",
        user_message=user_message,
        sink=lambda _event: None,
    )


@pytest.mark.asyncio
async def test_grounded_turn_populates_citations() -> None:
    knowledge = InMemoryKnowledge()
    knowledge.ingest("SmooAI returns are accepted within 30 days of delivery.", "returns.md")

    result = await _run(knowledge)

    assert len(result.citations) == 1
    citation = result.citations[0]
    assert citation["id"] == "returns.md"
    assert citation["title"] == "returns.md"
    assert citation["snippet"] == "SmooAI returns are accepted within 30 days of delivery."
    assert "score" in citation
    # A non-URL source carries no `url` field (matches the TS server).
    assert "url" not in citation


@pytest.mark.asyncio
async def test_url_source_carries_url_field() -> None:
    knowledge = InMemoryKnowledge()
    knowledge.ingest("Returns are accepted within 30 days.", "https://smoo.ai/returns")

    result = await _run(knowledge)

    assert result.citations[0]["url"] == "https://smoo.ai/returns"


@pytest.mark.asyncio
async def test_snippet_truncated_to_max_chars() -> None:
    long_content = "x" * (CITATION_SNIPPET_MAX_CHARS + 100)
    knowledge = InMemoryKnowledge()
    knowledge.ingest(long_content, "long.md")

    result = await _run(knowledge, user_message="x")

    assert result.citations[0]["snippet"] == long_content[:CITATION_SNIPPET_MAX_CHARS]
    assert len(result.citations[0]["snippet"]) == CITATION_SNIPPET_MAX_CHARS


@pytest.mark.asyncio
async def test_no_knowledge_yields_no_citations() -> None:
    result = await _run(knowledge=None)
    assert result.citations == []
