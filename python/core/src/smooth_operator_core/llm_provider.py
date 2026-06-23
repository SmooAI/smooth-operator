"""An ``LlmProvider`` seam over the LLM call so the agentic loop can be unit-tested
deterministically, without a live model or network.

The agent already takes an injected OpenAI-compatible chat client (the ``openai``
SDK pointed at a gateway). This module *formalizes* that seam as a
:class:`LlmProvider` Protocol — any object exposing the duck-typed
``chat.completions.create(...)`` surface already satisfies it, so the existing
``SmoothAgent`` constructor is unchanged and backward compatible.

It also ships a reusable, exported :class:`MockLlmProvider` that replaces the
ad-hoc fake clients the tests rolled by hand. The mock:

* is constructed with a script of responses — plain text, tool-call responses,
  and errors;
* returns them in FIFO order across calls;
* records each request (the messages + tool specs it was given) so a test can
  assert on what the agent sent.

This mirrors the BEHAVIOR of the Rust reference's ``MockLlmClient``
(``rust/smooth-operator-core/src/llm_provider.rs``). The Rust reference also
exposes streaming (``chat_stream``) and structured-output (``chat_structured``)
methods; the Python/TS/Go cores' agent loop only uses the single non-streaming
chat call, so the provider seam here covers that one surface. Streaming /
structured-output land when those features land in this engine.
"""

from __future__ import annotations

from types import SimpleNamespace
from typing import Any, Protocol, runtime_checkable


@runtime_checkable
class LlmProvider(Protocol):
    """The LLM call surface the agent loop depends on.

    This is exactly the slice of the ``openai`` async client the agent uses:
    ``provider.chat.completions.create(model=..., messages=..., tools=..., ...)``
    returning an OpenAI-shaped response (``.choices[0].message`` with
    ``.content`` / ``.tool_calls``, and an optional ``.usage``).

    Production wires the real ``openai.AsyncOpenAI`` client (which satisfies this
    structurally); tests inject a :class:`MockLlmProvider`.
    """

    chat: Any


# ── response builders (handy for scripting the mock and for assertions) ──────


def text_response(content: str) -> SimpleNamespace:
    """An OpenAI-shaped assistant message that is plain text (no tool calls)."""
    return SimpleNamespace(content=content, tool_calls=None)


def tool_call_response(call_id: str, name: str, arguments: str) -> SimpleNamespace:
    """An OpenAI-shaped assistant message that requests a single tool call.

    ``arguments`` is the raw JSON-string the model emits for the call's arguments
    (mirroring the wire shape the agent parses).
    """
    tool_call = SimpleNamespace(id=call_id, function=SimpleNamespace(name=name, arguments=arguments))
    return SimpleNamespace(content=None, tool_calls=[tool_call])


class RecordedCall:
    """One request the mock received, captured for assertions.

    ``messages`` and ``tools`` are the exact kwargs the agent passed to
    ``chat.completions.create`` for this call.
    """

    __slots__ = ("messages", "tools", "kwargs")

    def __init__(self, kwargs: dict[str, Any]) -> None:
        self.kwargs = kwargs
        self.messages: list[dict[str, Any]] = list(kwargs.get("messages") or [])
        self.tools: list[dict[str, Any]] | None = kwargs.get("tools")

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        return f"RecordedCall(messages={self.messages!r}, tools={self.tools!r})"


class _ScriptedError(Exception):
    """Marker for an error the script wants raised from a chat call."""


class _Completions:
    """Implements the ``chat.completions`` surface: replay + record."""

    def __init__(self, owner: MockLlmProvider) -> None:
        self._owner = owner

    async def create(self, **kwargs: Any) -> SimpleNamespace:
        self._owner._calls.append(RecordedCall(kwargs))
        if not self._owner._script:
            # Empty script: a benign terminal text response so loops don't hang.
            message: Any = text_response("")
        else:
            message = self._owner._script.pop(0)
        if isinstance(message, _ScriptedError):
            raise message
        return SimpleNamespace(choices=[SimpleNamespace(message=message)], usage=None)


class MockLlmProvider:
    """A deterministic :class:`LlmProvider` for tests.

    Script the responses it should return (FIFO), drive your code, then assert on
    :attr:`calls`. Construct with an optional list of scripted outcomes, or build
    it up fluently with :meth:`push_text` / :meth:`push_tool_call` /
    :meth:`push_error`.

    Example::

        mock = MockLlmProvider()
        mock.push_text("hello there")
        agent = SmoothAgent(mock, AgentOptions())
        result = await agent.run("hi")
        assert result.text == "hello there"
        assert mock.call_count == 1
        assert mock.calls[0].messages[-1]["content"] == "hi"
    """

    def __init__(self, script: list[Any] | None = None) -> None:
        self._script: list[Any] = list(script or [])
        self._calls: list[RecordedCall] = []
        self.chat = SimpleNamespace(completions=_Completions(self))

    # ── scripting (fluent: each returns self) ────────────────────────────────

    def push_response(self, message: Any) -> MockLlmProvider:
        """Queue a raw OpenAI-shaped assistant message for the next call."""
        self._script.append(message)
        return self

    def push_text(self, content: str) -> MockLlmProvider:
        """Queue a plain-text response for the next call."""
        return self.push_response(text_response(content))

    def push_tool_call(self, call_id: str, name: str, arguments: str) -> MockLlmProvider:
        """Queue a single-tool-call response for the next call."""
        return self.push_response(tool_call_response(call_id, name, arguments))

    def push_error(self, message: str) -> MockLlmProvider:
        """Queue an error to be raised on the next call."""
        self._script.append(_ScriptedError(message))
        return self

    # ── recordings ───────────────────────────────────────────────────────────

    @property
    def calls(self) -> list[RecordedCall]:
        """Every request the mock has received so far, in order."""
        return self._calls

    @property
    def call_count(self) -> int:
        """Number of requests received."""
        return len(self._calls)

    @property
    def last_call(self) -> RecordedCall | None:
        """The most recent request, if any."""
        return self._calls[-1] if self._calls else None
