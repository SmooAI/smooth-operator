"""Runtime validation against the spec JSON Schemas, using ``jsonschema``
(Draft 2020-12).

The spec ships self-contained draft-2020-12 schemas with internal ``$defs`` (no
cross-file ``$ref``\\ s). We register every schema under its ``$id`` in a
:class:`referencing.Registry` so any ``#/$defs/...`` pointer resolves, then expose:

  * :meth:`ProtocolValidator.validate_at` — validate against a spec-relative ref like
    ``events/stream-chunk.schema.json`` or
    ``actions/send-message.schema.json#/$defs/Request`` (the exact form used by
    ``conformance/fixtures.json``).
  * :meth:`ProtocolValidator.validate_event` / :meth:`validate_action` — convenience
    validators that pick the right schema from a frame's discriminator.

Schemas are loaded from the spec directory on disk. This module is intended for
build/test/server use; the wire client does not import it — validation is opt-in.
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path

from jsonschema import Draft202012Validator
from jsonschema.protocols import Validator
from referencing import Registry, Resource
from referencing.jsonschema import DRAFT202012

# Default spec dir: ../../spec relative to this file (python/src/smooth_operator/ -> spec).
DEFAULT_SPEC_DIR = Path(__file__).resolve().parent.parent.parent.parent / "spec"

_SUBDIRS = ["", "actions", "events", "domain"]

# Maps an event ``type`` to its schema file (spec-relative).
_EVENT_SCHEMA_FILE: dict[str, str] = {
    "immediate_response": "events/immediate-response.schema.json",
    "eventual_response": "events/eventual-response.schema.json",
    "stream_chunk": "events/stream-chunk.schema.json",
    "stream_token": "events/stream-token.schema.json",
    "keepalive": "events/keepalive.schema.json",
    "write_confirmation_required": "events/write-confirmation-required.schema.json",
    "otp_verification_required": "events/otp-verification-required.schema.json",
    "otp_sent": "events/otp-sent.schema.json",
    "otp_verified": "events/otp-verified.schema.json",
    "otp_invalid": "events/otp-invalid.schema.json",
    "error": "events/error.schema.json",
    "pong": "events/pong.schema.json",
}

# Maps an action ``action`` to its request schema ref (spec-relative).
_ACTION_SCHEMA_REF: dict[str, str] = {
    "create_conversation_session": "actions/create-conversation-session.schema.json#/$defs/Request",
    "send_message": "actions/send-message.schema.json#/$defs/Request",
    "get_session": "actions/get-session.schema.json#/$defs/Request",
    "get_conversation_messages": "actions/get-messages.schema.json#/$defs/Request",
    "confirm_tool_action": "actions/confirm-tool-action.schema.json#/$defs/Request",
    "verify_otp": "actions/verify-otp.schema.json#/$defs/Request",
    "ping": "actions/ping.schema.json#/$defs/Request",
}


@dataclass
class ValidationResult:
    valid: bool
    errors: list[str] = field(default_factory=list)


class ProtocolValidator:
    """Validates frames/instances against the on-disk spec schemas."""

    def __init__(self, registry: Registry, file_to_id: dict[str, str]) -> None:
        self._registry = registry
        self._file_to_id = file_to_id
        self._cache: dict[str, Validator] = {}

    @classmethod
    def load(cls, spec_dir: Path | str = DEFAULT_SPEC_DIR) -> ProtocolValidator:
        """Load every ``*.schema.json`` under ``spec_dir`` and register it."""
        spec_dir = Path(spec_dir)
        registry = Registry()
        file_to_id: dict[str, str] = {}

        for sub in _SUBDIRS:
            d = spec_dir / sub if sub else spec_dir
            for p in sorted(d.glob("*.schema.json")):
                rel = f"{sub}/{p.name}" if sub else p.name
                schema = json.loads(p.read_text())
                schema_id = schema.get("$id") or f"urn:smooth-agent:{rel}"
                resource = Resource(contents=schema, specification=DRAFT202012)
                registry = registry.with_resource(uri=schema_id, resource=resource)
                file_to_id[rel] = schema_id

        return cls(registry, file_to_id)

    def validate_at(self, schema_ref: str, instance: object) -> ValidationResult:
        """Validate ``instance`` against a spec-relative schema ref. The ref is the
        form used in ``fixtures.json``: a file path, optionally with a JSON-pointer
        fragment into the schema's ``$defs`` (e.g.
        ``actions/ping.schema.json#/$defs/Request``)."""
        validator = self._compile(schema_ref)
        errors = sorted(validator.iter_errors(instance), key=lambda e: e.path)
        if not errors:
            return ValidationResult(valid=True)
        return ValidationResult(
            valid=False,
            errors=[f"{_json_path(e)} {e.message}".strip() for e in errors],
        )

    def validate_event(self, frame: dict) -> ValidationResult:
        """Validate a server event frame by selecting the schema from its ``type``."""
        file = _EVENT_SCHEMA_FILE.get(frame.get("type"))
        if file is None:
            return ValidationResult(False, [f"Unknown event type: {frame.get('type')!r}"])
        return self.validate_at(file, frame)

    def validate_action(self, frame: dict) -> ValidationResult:
        """Validate a client action frame by selecting the schema from its ``action``."""
        ref = _ACTION_SCHEMA_REF.get(frame.get("action"))
        if ref is None:
            return ValidationResult(False, [f"Unknown action: {frame.get('action')!r}"])
        return self.validate_at(ref, frame)

    def _compile(self, schema_ref: str) -> Validator:
        cached = self._cache.get(schema_ref)
        if cached is not None:
            return cached

        file, _, pointer = schema_ref.partition("#")
        schema_id = self._file_to_id.get(file)
        if schema_id is None:
            raise ValueError(f'No schema registered for "{file}" (ref "{schema_ref}")')

        uri = f"{schema_id}#{pointer}" if pointer else f"{schema_id}#"
        # A ``$ref``-only schema lets jsonschema resolve the target (incl. any
        # ``#/$defs/...`` JSON-pointer fragment) through the registry of $id-keyed
        # spec schemas.
        validator = Draft202012Validator({"$ref": uri}, registry=self._registry)
        self._cache[schema_ref] = validator
        return validator


def _json_path(error) -> str:
    parts = [str(p) for p in error.absolute_path]
    return "/" + "/".join(parts) if parts else "<root>"


def format_errors(errors: list[str]) -> str:
    return "; ".join(errors)
