"""OpenTelemetry GenAI instrumentation for the Python server's agent turn.

The Python sibling of the Rust ``smooth_operator::telemetry`` module + the
``gen_ai.chat`` / ``gen_ai.tool`` span points in ``runner.rs``. It defines the
GenAI semantic-convention attribute names (the same strings the Rust/TS servers
emit, so the observability studio groups every engine's turns together) and
installs a tracer provider at server boot.

## Span shape (mirrors ``run_streaming_turn``)
:meth:`TurnRunner.run` opens a span named :data:`SPAN_CHAT` (``gen_ai.chat``) per
turn, carrying :data:`GEN_AI_SYSTEM`, :data:`GEN_AI_REQUEST_MODEL`,
:data:`GEN_AI_CONVERSATION_ID`, :data:`GEN_AI_AGENT_NAME`, :data:`SMOOAI_ORG_ID`,
and — on completion — :data:`GEN_AI_USAGE_INPUT_TOKENS` /
:data:`GEN_AI_USAGE_OUTPUT_TOKENS` when the engine reported token usage. Each
tool call opens a child :data:`SPAN_TOOL` (``gen_ai.tool``) carrying
:data:`GEN_AI_TOOL_NAME` and the redacted :data:`GEN_AI_TOOL_ARGUMENTS`.

## Exporter gating (no collector needed for tests/binaries)
:func:`init_telemetry` installs an OTLP exporter **only** when
``OTEL_EXPORTER_OTLP_ENDPOINT`` is set (and the optional ``otel`` extra is
installed). Unset ⇒ no provider is installed, so the binary and the test suite
run with zero external dependencies and span calls are cheap no-ops. Idempotent.
"""

from __future__ import annotations

import json
import logging
import os

from opentelemetry import trace

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# GenAI semantic-convention attribute keys. Byte-for-byte the Rust constants in
# smooth-operator-core's telemetry.rs so Python + Rust + TS turns interoperate.
# ---------------------------------------------------------------------------

#: ``gen_ai.system`` — the GenAI system / provider name.
GEN_AI_SYSTEM = "gen_ai.system"
#: ``gen_ai.request.model`` — the model requested for the turn.
GEN_AI_REQUEST_MODEL = "gen_ai.request.model"
#: ``gen_ai.conversation.id`` — the conversation this turn belongs to.
GEN_AI_CONVERSATION_ID = "gen_ai.conversation.id"
#: ``gen_ai.usage.input_tokens`` — prompt tokens consumed by the turn.
GEN_AI_USAGE_INPUT_TOKENS = "gen_ai.usage.input_tokens"
#: ``gen_ai.usage.output_tokens`` — completion tokens produced by the turn.
GEN_AI_USAGE_OUTPUT_TOKENS = "gen_ai.usage.output_tokens"
#: ``gen_ai.tool.name`` — the name of an invoked tool.
GEN_AI_TOOL_NAME = "gen_ai.tool.name"
#: ``gen_ai.tool.call.arguments`` — the (redacted) JSON arguments passed to a tool.
GEN_AI_TOOL_ARGUMENTS = "gen_ai.tool.call.arguments"
#: ``gen_ai.agent.name`` — the agent/persona driving the turn.
GEN_AI_AGENT_NAME = "gen_ai.agent.name"
#: ``smooai.org_id`` — the owning org. Matches the monorepo TS chat handler so the
#: observability studio groups Python + Rust + TS turns by org.
SMOOAI_ORG_ID = "smooai.org_id"

#: The value emitted for :data:`GEN_AI_SYSTEM` — identifies these traces.
SYSTEM_NAME = "smooth-operator"
#: The agent name the reference streaming path builds its ``AgentConfig`` with;
#: emitted as :data:`GEN_AI_AGENT_NAME` on the turn span.
AGENT_NAME = "smooth-agent-chat"

#: Span name for the per-turn GenAI chat span (``gen_ai.chat``).
SPAN_CHAT = "gen_ai.chat"
#: Span name for a per-tool-call child span (``gen_ai.tool``).
SPAN_TOOL = "gen_ai.tool"

#: Env var that switches :func:`init_telemetry` to a real OTLP exporter.
OTLP_ENDPOINT_ENV = "OTEL_EXPORTER_OTLP_ENDPOINT"

#: Max length of a serialized tool-arguments string recorded on a span, so a
#: pathological argument blob can't bloat span export.
MAX_TOOL_ARGS_LEN = 2048

#: Object key-name substrings whose values are scrubbed before landing on a span.
_SECRET_NEEDLES = (
    "secret",
    "token",
    "password",
    "api_key",
    "apikey",
    "authorization",
    "bearer",
    "credential",
    "access_key",
    "private_key",
)


def _redact_in_place(value: object) -> None:
    """Recursively replace secret-named object values with ``"[REDACTED]"``."""
    if isinstance(value, dict):
        for key, sub in value.items():
            if isinstance(key, str) and any(n in key.lower() for n in _SECRET_NEEDLES):
                value[key] = "[REDACTED]"
            else:
                _redact_in_place(sub)
    elif isinstance(value, list):
        for item in value:
            _redact_in_place(item)


def redact_tool_arguments(arguments: str) -> str:
    """Redact a tool's serialized JSON arguments for span recording.

    Best-effort scrub keyed on argument *names* (mirrors the Rust
    ``redact_tool_arguments``): the value of any object key whose name looks
    secret-bearing is replaced with ``"[REDACTED]"``. Non-JSON input passes
    through as-is. The result is always length-capped at :data:`MAX_TOOL_ARGS_LEN`.
    """
    try:
        parsed = json.loads(arguments)
    except (json.JSONDecodeError, TypeError):
        redacted = arguments
    else:
        _redact_in_place(parsed)
        redacted = json.dumps(parsed, separators=(",", ":"))
    if len(redacted) > MAX_TOOL_ARGS_LEN:
        return redacted[:MAX_TOOL_ARGS_LEN] + "…"
    return redacted


def tracer() -> trace.Tracer:
    """The server's GenAI tracer. Returns a no-op tracer until (and unless)
    :func:`init_telemetry` installs a provider — span calls are then cheap."""
    return trace.get_tracer(SYSTEM_NAME)


_initialized = False


def init_telemetry() -> bool:
    """Install tracing → OpenTelemetry for the process. Idempotent.

    - ``OTEL_EXPORTER_OTLP_ENDPOINT`` set ⇒ install an OTLP span exporter behind a
      batch processor on a global ``TracerProvider`` (service.name = the system
      name). Requires the optional ``otel`` extra; if the OTLP exporter isn't
      installed, logs a warning and leaves the no-op provider in place rather than
      crashing the server (a bad/absent exporter must never take down the agent).
    - unset ⇒ no provider installed (local no-op, zero external deps).

    Returns ``True`` if this call performed the install, ``False`` otherwise.
    """
    global _initialized
    if _initialized:
        return False

    endpoint = (os.environ.get(OTLP_ENDPOINT_ENV) or "").strip()
    if not endpoint:
        logger.debug("telemetry: %s unset; no OTLP exporter installed", OTLP_ENDPOINT_ENV)
        return False

    try:
        from opentelemetry.exporter.otlp.proto.grpc.trace_exporter import OTLPSpanExporter
        from opentelemetry.sdk.resources import Resource
        from opentelemetry.sdk.trace import TracerProvider
        from opentelemetry.sdk.trace.export import BatchSpanProcessor
    except ImportError as exc:  # pragma: no cover - exercised only without the extra
        logger.warning(
            "telemetry: %s set but the OTLP exporter is unavailable (%s); "
            "install the 'otel' extra. Running with no exporter.",
            OTLP_ENDPOINT_ENV,
            exc,
        )
        return False

    provider = TracerProvider(resource=Resource.create({"service.name": SYSTEM_NAME}))
    provider.add_span_processor(BatchSpanProcessor(OTLPSpanExporter(endpoint=endpoint)))
    trace.set_tracer_provider(provider)
    _initialized = True
    logger.info("telemetry: OTLP exporter installed (endpoint=%s)", endpoint)
    return True
