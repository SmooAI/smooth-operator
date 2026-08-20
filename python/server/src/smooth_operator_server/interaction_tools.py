"""Per-turn Rich Interaction tools — the model-facing surface, built fresh each turn.

Two model-callable tools, mirroring the Rust core crate (``tools/interaction.rs``):

- **``request_<kind>``** (one per hosted kind): the raise tool. On a **rich** channel
  (the kind's capability is in the session's ``supports``) it parks the turn — registers
  a park, emits ``interaction_required``, and awaits the client's ``submit_interaction``
  — returning the kind's canonical payload. On a **fallback** channel it returns the
  kind's conversational directive immediately (no park) and stashes the spec so the
  generic submit tool can validate it fully.
- **``submit_interaction``** (one, registered only when ≥1 kind is on the fallback path):
  the conversational-path submit. Routes the model's values to the kind's validator and
  returns the same canonical payload — or a tool error the model relays and re-asks on.

The park is folded straight into the raise tool's coroutine (an ``await`` on the park
future), so — unlike the Rust bridge task + mpsc channel — no extra task is needed: the
turn coroutine suspends on the future and the connection read loop stays free to receive
the ``submit_interaction`` action. Same shape as the write-confirmation park.
"""

from __future__ import annotations

import asyncio
import json
import uuid
from typing import TYPE_CHECKING, Any, Callable

from smooth_operator_core import FunctionTool

from . import protocol
from .interaction import (
    INTERACTION_TIMEOUT,
    InteractionKind,
    InteractionOutcome,
    InteractionRegistry,
    PendingInteractions,
)

if TYPE_CHECKING:
    from .session_store import SessionStore

#: The sink type the raise tool emits ``interaction_required`` through.
Sink = Callable[[dict[str, Any]], None]

#: submit_interaction TOOL messages — byte-for-byte the Rust ``SubmitInteractionTool``.
_SUBMIT_DECLINED_MESSAGE = "Noted. Continue helping the visitor without this and do not ask again this conversation."


def _request_tool(
    kind: InteractionKind,
    rich: bool,
    session_id: str,
    request_id: str,
    sink: Sink,
    pending: PendingInteractions,
    raised_specs: dict[str, dict[str, Any]],
) -> FunctionTool:
    """Build one ``request_<kind>`` raise tool bound to this turn's session/sink."""
    schema = kind.tool_schema()

    async def _run(args: dict[str, Any]) -> str:
        # The stream loop DEFERRED this tool's toolCall chunk (see
        # ``TurnRunner._is_interaction_raise``) so the park path can emit it AFTER
        # ``interaction_required`` — the reference order, and the same order the
        # write-confirmation park already uses. Every non-park exit emits it here.
        # (the state shape is inlined rather than imported from `turn_runner`, which
        # imports this module — `_tool_call_state_from`'s one-line dict, not a cycle.)
        def emit_call() -> None:
            state = {"rawResponse": {"toolCall": {"name": schema["name"], "arguments": args}}}
            sink(protocol.stream_chunk(request_id, schema["name"], state))

        try:
            request = kind.parse_request(args)  # ValueError → surfaced to the model
        except Exception:
            emit_call()
            raise
        if not rich:
            # Fallback: no card can render — hand the model the conversational directive
            # and stash the spec so the submit tool validates with full required-ness.
            raised_specs[request.kind] = request.spec
            emit_call()
            return json.dumps(
                {
                    "mode": "conversational",
                    "kind": request.kind,
                    "spec": request.spec,
                    "reason": request.reason,
                    "instructions": kind.fallback_directive(request.spec, request.reason),
                }
            )
        # Rich: park. Register the park, emit the event, await the client's submit.
        interaction_id = str(uuid.uuid4())
        future = pending.register(session_id, interaction_id, request.kind, request.spec)
        sink(protocol.interaction_required(request_id, interaction_id, request.kind, request.spec, request.reason))
        emit_call()
        try:
            outcome = await asyncio.wait_for(future, INTERACTION_TIMEOUT)
        except (asyncio.TimeoutError, asyncio.CancelledError):
            # Our own timeout / turn teardown reads as no answer to the card. Drop any
            # lingering park so a late submit can't resolve a dead future.
            pending.clear(session_id)
            outcome = InteractionOutcome.no_response()
        return json.dumps(outcome.to_payload())

    return FunctionTool(
        name=schema["name"],
        description=schema["description"],
        parameters=schema["parameters"],
        func=_run,
    )


def _submit_tool(
    kinds: InteractionRegistry,
    raised_specs: dict[str, dict[str, Any]],
    store: SessionStore,
    session_id: str,
) -> FunctionTool:
    """Build the generic ``submit_interaction`` tool (conversational-fallback submit)."""
    hosted = [k.kind() for k in kinds.kinds()]

    async def _run(args: dict[str, Any]) -> str:
        kind_id = args.get("kind")
        kind = kinds.get(kind_id) if isinstance(kind_id, str) else None
        if kind is None:
            raise ValueError(f"unknown interaction kind {kind_id!r}; hosted kinds: {', '.join(hosted)}")
        if args.get("declined") is True:
            return json.dumps({"status": "declined", "message": _SUBMIT_DECLINED_MESSAGE})
        values = args.get("values")
        if not isinstance(values, dict):
            raise ValueError("'values' object is required unless declined=true")
        # The raise may have happened this turn (spec stashed) or a prior turn (spec gone
        # → format-only validation).
        spec = raised_specs.get(kind_id)
        canonical, errors = kind.validate(spec, values)
        if errors:
            detail = "; ".join(f"{e.field}: {e.message}" for e in errors)
            raise ValueError(
                f"validation failed — {detail}. Re-ask the visitor for the corrected value(s) and submit again."
            )
        # Run the kind's host effect on the conversational path too — the SAME kind-agnostic
        # seam the rich path fires (a no-op for choices; identity_intake stamps the session's
        # contacts), mirroring the Rust InteractionConfig.attach callback.
        await kind.host_effect(store, session_id, canonical or {})
        return json.dumps({"status": "submitted", "values": canonical})

    return FunctionTool(
        name="submit_interaction",
        description=(
            "Submit the visitor's answer(s) to an interaction you raised conversationally (a "
            "request_<kind> tool told you the channel cannot render a card). Provide the `kind` and "
            "the `values` for that kind, or `declined: true` if the visitor refused. Returns the "
            "validated result, or an error naming what to re-ask."
        ),
        parameters={
            "type": "object",
            "properties": {
                "kind": {"type": "string", "enum": hosted, "description": "The interaction kind being answered."},
                "values": {"type": "object", "description": "The kind-specific submitted values."},
                "declined": {"type": "boolean", "description": "True when the visitor refused the interaction."},
            },
            "required": ["kind"],
        },
        func=_run,
    )


def build_interaction_tools(
    kinds: InteractionRegistry,
    capabilities: set[str],
    session_id: str,
    request_id: str,
    sink: Sink,
    pending: PendingInteractions,
    store: "SessionStore",
) -> list[FunctionTool]:
    """Build this turn's Rich Interaction tools: one ``request_<kind>`` per hosted kind
    (rich when the kind's capability is declared, else fallback), plus the generic
    ``submit_interaction`` tool when at least one kind is on the fallback path. Mirrors the
    Rust runner's per-turn registration + ``any_fallback`` gate. ``store`` is threaded through
    so a valid conversational-fallback submit can run the kind's host effect on the session."""
    raised_specs: dict[str, dict[str, Any]] = {}
    tools: list[FunctionTool] = []
    any_fallback = False
    for kind in kinds.kinds():
        rich = kind.capability() in capabilities
        any_fallback = any_fallback or not rich
        tools.append(_request_tool(kind, rich, session_id, request_id, sink, pending, raised_specs))
    if any_fallback:
        tools.append(_submit_tool(kinds, raised_specs, store, session_id))
    return tools
