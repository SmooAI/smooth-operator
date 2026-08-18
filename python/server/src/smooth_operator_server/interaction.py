"""Rich Interactions — the extensible structured-interaction framework.

One pattern, many kinds (see the Rust reference ``rust/smooth-operator/src/interaction.rs``
and ``docs/Architecture/Rich Interactions.md``): an agent raises a **structured
interaction** (identity intake, a date picker, choice chips, …). On a channel whose
client declared the kind's render capability (``supports`` at
``create_conversation_session``), the turn parks and the client renders a rich card
(``interaction_required`` → ``submit_interaction``). On a text-only channel the same
raise degrades to the kind's **conversational fallback**: a directive the model follows
turn by turn, submitting through the generic ``submit_interaction`` *tool*. Both paths
run the kind's **server-side validator** and resume the turn with the same canonical
payload.

Adding a kind = implementing :class:`InteractionKind` (one module) and registering it in
an :class:`InteractionRegistry`. No new protocol events, no new client verbs.

The park/resume half (:class:`PendingInteractions`) is the kind-agnostic generalization
of :mod:`smooth_operator_server.confirmation`: where the confirmation registry parks a
turn on a ``bool`` verdict keyed by session, this parks a turn on an
:class:`InteractionOutcome` keyed by session — the async analog of the Rust server's
``PendingInteraction`` map (``register_interaction`` / ``pending_interaction`` /
``take_interaction`` / ``clear_interaction``).
"""

from __future__ import annotations

import asyncio
from abc import ABC, abstractmethod
from dataclasses import dataclass, field
from typing import Any

#: How long a parked interaction waits for the client's ``submit_interaction`` before the
#: raise tool unblocks with a ``no_response`` payload. Matches the Rust server's
#: ``INTERACTION_TIMEOUT`` and the write-confirmation window (300s).
INTERACTION_TIMEOUT = 300.0

#: The tool-result guidance the raise tool returns for each non-submitted outcome —
#: byte-for-byte the Rust ``RequestInteractionTool::execute`` messages, so the model
#: reads the same instruction on every engine.
DECLINED_MESSAGE = "The visitor declined. Continue helping them without this and do not ask again this conversation."
NO_RESPONSE_MESSAGE = (
    "The visitor did not respond to the card. Continue without it; you may offer again later if it becomes relevant."
)


@dataclass(frozen=True)
class InteractionFieldError:
    """A single per-field validation failure, carried on ``interaction_invalid`` and in
    the conversational tool's error result. ``field`` is a kind-specific key (for
    ``choices`` it is the question ``header``)."""

    field: str
    message: str

    def to_dict(self) -> dict[str, str]:
        return {"field": self.field, "message": self.message}


@dataclass(frozen=True)
class InteractionRequest:
    """A parsed raise: what the agent asked the visitor for. ``spec`` shape per
    ``spec/interactions/<kind>.schema.json#/$defs/Spec``."""

    kind: str
    spec: dict[str, Any]
    reason: str


@dataclass(frozen=True)
class InteractionOutcome:
    """How a parked interaction resolved — the value the raise tool's awaited future
    settles to. ``status`` is one of ``submitted`` / ``declined`` / ``no_response``
    (the ``Payload.status`` enum); ``values`` is present only when submitted."""

    status: str
    values: dict[str, Any] | None = None

    @classmethod
    def submitted(cls, values: dict[str, Any]) -> InteractionOutcome:
        return cls("submitted", values)

    @classmethod
    def declined(cls) -> InteractionOutcome:
        return cls("declined")

    @classmethod
    def no_response(cls) -> InteractionOutcome:
        return cls("no_response")

    def to_payload(self) -> dict[str, Any]:
        """The canonical validated payload the parked turn resumes with — the string the
        raise tool hands back to the model (identical on the card and conversational
        paths). Matches ``spec/interactions/choices.schema.json#/$defs/Payload``."""
        if self.status == "submitted":
            return {"status": "submitted", "values": self.values or {"answers": []}}
        if self.status == "declined":
            return {"status": "declined", "message": DECLINED_MESSAGE}
        return {"status": "no_response", "message": NO_RESPONSE_MESSAGE}


class InteractionKind(ABC):
    """One interaction kind — the extension seam of the Rich Interactions pattern.

    A kind supplies exactly the pieces that differ per interaction; ALL park / resume /
    event / registry machinery is shared and kind-agnostic: identity (:meth:`kind` /
    :meth:`capability`), the LLM-facing raise-tool surface (:meth:`tool_schema` +
    :meth:`parse_request`), the server-side :meth:`validate`, and the conversational
    :meth:`fallback_directive`."""

    @abstractmethod
    def kind(self) -> str:
        """The wire kind id (e.g. ``choices``). Selects the client card + the validator."""

    @abstractmethod
    def capability(self) -> str:
        """The client render capability that gates the rich path (e.g. ``choice_chips``).
        A session that declared it in ``supports`` gets the parked card; anything else
        gets the conversational fallback."""

    @abstractmethod
    def tool_schema(self) -> dict[str, Any]:
        """The raise tool's LLM-facing schema — ``{name, description, parameters}``.
        Convention: name it ``request_<kind>``."""

    @abstractmethod
    def parse_request(self, args: dict[str, Any]) -> InteractionRequest:
        """Parse + canonicalize the raise tool's arguments into the kind's ``spec`` and
        the human-readable reason. Raises :class:`ValueError` on malformed arguments (the
        engine surfaces the text to the model)."""

    @abstractmethod
    def validate(
        self, spec: dict[str, Any] | None, values: dict[str, Any]
    ) -> tuple[dict[str, Any] | None, list[InteractionFieldError]]:
        """Validate ``values`` against ``spec``, returning ``(canonical, [])`` on success
        or ``(None, errors)`` with EVERY failed field (so a card annotates all in one
        round-trip). ``spec`` may be ``None`` on the conversational path when the raise
        happened in an earlier turn — the kind then applies format-only validation."""

    @abstractmethod
    def fallback_directive(self, spec: dict[str, Any], reason: str) -> str:
        """The conversational-degradation directive for text-only channels: instructions
        the model follows to collect the same information turn by turn, then submit
        through the ``submit_interaction`` tool."""


class InteractionRegistry:
    """The catalog of interaction kinds a server hosts. The default catalog is the
    reference kind (``choices``); a host may extend or replace it. The Python analog of
    the Rust ``InteractionRegistry``."""

    def __init__(self) -> None:
        self._kinds: dict[str, InteractionKind] = {}

    def with_kind(self, kind: InteractionKind) -> InteractionRegistry:
        """Register a kind (builder). A later registration with the same id replaces the
        earlier one."""
        self._kinds[kind.kind()] = kind
        return self

    def get(self, kind: str) -> InteractionKind | None:
        return self._kinds.get(kind)

    def kinds(self) -> list[InteractionKind]:
        """Every registered kind, in registration order."""
        return list(self._kinds.values())

    @classmethod
    def default(cls) -> InteractionRegistry:
        """The reference catalog: ``choices``."""
        from .choices import ChoicesKind

        return cls().with_kind(ChoicesKind())


@dataclass
class _Pending:
    """One parked interaction: the id echoed on submit, the kind + spec the submit
    validates against, and the future the raise tool awaits."""

    interaction_id: str
    kind: str
    spec: dict[str, Any]
    future: asyncio.Future[InteractionOutcome]


@dataclass
class PendingInteractions:
    """The session-keyed registry of parked interactions — the kind-agnostic
    generalization of :class:`~smooth_operator_server.confirmation.ConfirmationRegistry`.

    Single-threaded under the asyncio event loop (every method runs on the loop, so the
    dict needs no locking). At most one outstanding interaction per session; a new park
    supersedes any prior one (its turn unblocks with ``no_response``)."""

    _pending: dict[str, _Pending] = field(default_factory=dict)

    def register(self, session_id: str, interaction_id: str, kind: str, spec: dict[str, Any]) -> asyncio.Future[InteractionOutcome]:
        """Register (and return) a fresh outcome future for ``session_id``. Any prior park
        is superseded — resolved ``no_response`` so its raise tool can never dangle
        (mirrors the Rust ``register_interaction`` replacing the prior responder)."""
        prior = self._pending.pop(session_id, None)
        if prior is not None and not prior.future.done():
            prior.future.set_result(InteractionOutcome.no_response())
        future: asyncio.Future[InteractionOutcome] = asyncio.get_running_loop().create_future()
        self._pending[session_id] = _Pending(interaction_id, kind, spec, future)
        return future

    def peek(self, session_id: str) -> _Pending | None:
        """The parked interaction for ``session_id`` WITHOUT consuming it — the submit
        handler peeks to validate, so an invalid submit leaves the turn parked."""
        return self._pending.get(session_id)

    def resolve(self, session_id: str, outcome: InteractionOutcome) -> bool:
        """Consume the park for ``session_id`` and settle its future with ``outcome``.
        Returns ``True`` if a park was resolved, ``False`` if none was awaiting (a
        duplicate/stale submit). Taking it out makes a duplicate a clean no-op (mirrors
        the Rust ``take_interaction``)."""
        pending = self._pending.pop(session_id, None)
        if pending is None or pending.future.done():
            return False
        pending.future.set_result(outcome)
        return True

    def clear(self, session_id: str) -> None:
        """Drop any park for ``session_id`` (turn ended). Idempotent. Resolves a still-
        pending future ``no_response`` so a raise tool awaiting at teardown unblocks."""
        pending = self._pending.pop(session_id, None)
        if pending is not None and not pending.future.done():
            pending.future.set_result(InteractionOutcome.no_response())

    def reject_all(self) -> None:
        """Resolve every outstanding park ``no_response`` (connection torn down) so any
        turn parked on an interaction unparks and finishes cleanly — fail soft (the agent
        continues without the answer), never leave a turn hung."""
        for pending in tuple(self._pending.values()):
            if not pending.future.done():
                pending.future.set_result(InteractionOutcome.no_response())
        self._pending.clear()
