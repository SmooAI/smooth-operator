"""Identity intake — channel-normalized lead/identity capture: the first (reference)
**Rich Interaction kind** (see :mod:`interaction`, ``docs/Architecture/Rich Interactions.md``,
and the Rust reference ``rust/smooth-operator/src/identity_intake.rs``, mirrored exactly).

- On a channel that declared the ``identity_form`` capability, the agent's
  ``request_identity_intake`` tool parks the turn and the server emits
  ``interaction_required { kind: "identity_intake" }``; the client's form resumes with a
  ``submit_interaction`` action.
- On a **text-only** channel the same raise degrades to a conversational directive and the
  model submits the collected values through the generic ``submit_interaction`` *tool*.

Both paths validate through :func:`validate_intake` — one implementation, one behavior —
and resume the turn with the same structured payload. On a valid submit the kind's
:meth:`IdentityIntakeKind.host_effect` stamps the captured contacts onto the session
(``userName`` / ``contactEmail`` / ``contactPhone`` — the same keys the pre-chat/create path
stashes and the OTP contact seam reads), so a captured contact is immediately OTP-verifiable.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from .interaction import InteractionFieldError, InteractionKind, InteractionRequest

if TYPE_CHECKING:  # avoid a runtime import cycle (session_store never imports this module)
    from .session_store import SessionStore

#: The closed set of identity fields intake can collect.
_FIELD_KEYS = ("name", "email", "phone")


def normalize_email(raw: str) -> str | None:
    """Minimal email-shape validation: exactly one ``@``, non-empty local part, a
    dot-containing domain, no whitespace. Returns the trimmed address with a lowercased
    domain, or ``None`` when malformed. Mirrors ``identity_intake.rs::normalize_email``.

    ponytail: shape check, not RFC 5322 — deliverability is the host's email service's job,
    not the protocol boundary's."""
    s = raw.strip()
    if not s or any(c.isspace() for c in s):
        return None
    local, sep, domain = s.partition("@")
    if not sep or not local or "@" in domain:
        return None
    domain_lc = domain.lower()
    parts = domain_lc.split(".")
    # Domain needs an interior dot: `a.b`, not `.b`, `a.`, or `ab`.
    if len(parts) < 2 or any(p == "" for p in parts):
        return None
    return f"{local}@{domain_lc}"


def normalize_phone_e164(raw: str) -> str | None:
    """Normalize a phone number to E.164, or ``None`` when unparseable. Strips common
    separators (space, ``-``, ``.``, ``(``, ``)``), then accepts ``+`` + 8–15 digits
    (already E.164), or a bare 10-digit / 1-prefixed 11-digit NANP number → ``+1…``.
    Mirrors ``identity_intake.rs::normalize_phone_e164``.

    ponytail: NANP default for bare national numbers; swap in a phonenumber library if
    non-NANP national formats ever need to parse."""
    s = "".join(c for c in raw.strip() if c not in " -.()")
    plus = s.startswith("+")
    digits = s[1:] if plus else s
    if not digits or not digits.isdigit():
        return None
    if plus:
        # E.164: country code can't start with 0; total 8–15 digits.
        if 8 <= len(digits) <= 15 and not digits.startswith("0"):
            return f"+{digits}"
        return None
    if len(digits) == 10:
        return f"+1{digits}"
    if len(digits) == 11 and digits.startswith("1"):
        return f"+{digits}"
    return None


def validate_intake(
    fields: list[dict[str, Any]], values: dict[str, Any]
) -> tuple[dict[str, Any] | None, list[InteractionFieldError]]:
    """Validate raw submitted ``values`` against the requested ``fields``, returning
    ``(normalized_values, [])`` or ``(None, errors)`` with EVERY per-field failure (so a
    form annotates all of them in one round-trip). Mirrors ``identity_intake.rs::validate_intake``.

    Rules:
    - every ``required`` field must be present and non-blank;
    - ``name``: non-empty after trim;
    - ``email``: ``local@domain.tld`` shape (single ``@``, dot in the domain, no whitespace);
      domain lowercased;
    - ``phone``: E.164 after stripping separators.

    Fields that were NOT requested but are present are still validated and kept — a visitor
    volunteering their phone is a gift, not an error."""
    errors: list[InteractionFieldError] = []
    out: dict[str, Any] = {}

    def _get(key: str) -> str | None:
        v = values.get(key)
        return v if isinstance(v, str) else None

    # Required-ness: every required requested field must be present + non-blank.
    for field in fields:
        key = field.get("key")
        if field.get("required") and not (_get(key) or "").strip():
            errors.append(InteractionFieldError(key, "this field is required"))

    # Format validation + normalization for whatever was provided.
    name = (_get("name") or "").strip()
    if name:
        out["name"] = name

    email = (_get("email") or "").strip()
    if email:
        normalized = normalize_email(email)
        if normalized is not None:
            out["email"] = normalized
        else:
            errors.append(InteractionFieldError("email", "must be a valid email address"))

    phone = (_get("phone") or "").strip()
    if phone:
        normalized = normalize_phone_e164(phone)
        if normalized is not None:
            out["phone"] = normalized
        else:
            errors.append(
                InteractionFieldError(
                    "phone", "must be a valid phone number (include your country code, e.g. +1 555 123 4567)"
                )
            )

    if errors:
        return None, errors
    return out, []


def parse_fields(raw: Any) -> list[dict[str, Any]]:
    """Parse the raise tool's ``fields`` argument into validated field dicts. Accepts both
    the structured form (``[{ "key": "email", "required": true, "label": "Work email" }]``)
    and the shorthand the model likes to emit (``["email", "name"]`` — shorthand fields are
    ``required: true``). Unknown keys are an error (closed set). Raises :class:`ValueError`
    on malformed arguments. Mirrors ``identity_intake.rs::parse_fields``."""
    if not isinstance(raw, list):
        raise ValueError("'fields' must be an array")
    if not raw:
        raise ValueError("'fields' must contain at least one field")

    def _key(s: str) -> str:
        if s not in _FIELD_KEYS:
            raise ValueError(f"unknown intake field '{s}' (expected name, email, or phone)")
        return s

    fields: list[dict[str, Any]] = []
    for item in raw:
        if isinstance(item, str):
            fields.append({"key": _key(item), "required": True})
        elif isinstance(item, dict):
            key = item.get("key")
            if not isinstance(key, str):
                raise ValueError("each field object needs a string 'key'")
            field: dict[str, Any] = {"key": _key(key), "required": bool(item.get("required", True))}
            label = item.get("label")
            if isinstance(label, str) and label:
                field["label"] = label
            fields.append(field)
        else:
            raise ValueError(f"invalid field entry: {item!r}")
    return fields


class IdentityIntakeKind(InteractionKind):
    """The ``identity_intake`` Rich Interaction kind — structured name/email/phone lead
    capture (see the module docs and ``spec/interactions/identity-intake.schema.json``)."""

    def kind(self) -> str:
        return "identity_intake"

    def capability(self) -> str:
        return "identity_form"

    def tool_schema(self) -> dict[str, Any]:
        return {
            "name": "request_identity_intake",
            "description": (
                "Ask the visitor for their contact details (name, email, and/or phone) in a "
                "channel-appropriate way. On channels that can render a form the visitor fills a "
                "structured form; on text channels you will be told to collect the fields "
                "conversationally. Always use this tool instead of free-forming a request for "
                "contact details."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "fields": {
                        "type": "array",
                        "minItems": 1,
                        "description": (
                            "Which fields to collect, in order. Each entry is either a string "
                            '("name" | "email" | "phone") or an object { key, required?, label? }.'
                        ),
                        "items": {
                            "anyOf": [
                                {"type": "string", "enum": list(_FIELD_KEYS)},
                                {
                                    "type": "object",
                                    "properties": {
                                        "key": {"type": "string", "enum": list(_FIELD_KEYS)},
                                        "required": {"type": "boolean"},
                                        "label": {"type": "string"},
                                    },
                                    "required": ["key"],
                                },
                            ]
                        },
                    },
                    "reason": {
                        "type": "string",
                        "description": 'Why you need these details, phrased for the visitor (e.g. "to send you the quote").',
                    },
                },
                "required": ["fields", "reason"],
            },
        }

    def parse_request(self, args: dict[str, Any]) -> InteractionRequest:
        fields = parse_fields(args.get("fields"))
        reason = str(args.get("reason") or "").strip() or "to help you better"
        return InteractionRequest(kind=self.kind(), spec={"fields": fields}, reason=reason)

    def validate(
        self, spec: dict[str, Any] | None, values: dict[str, Any]
    ) -> tuple[dict[str, Any] | None, list[InteractionFieldError]]:
        # The spec's fields drive required-ness; a None/absent spec (fallback raise from an
        # earlier turn) degrades to format-only validation.
        fields = (spec or {}).get("fields") or []
        if not isinstance(values, dict):
            return None, [InteractionFieldError("values", "invalid values shape: expected an object")]
        if not any(isinstance(values.get(k), str) and values.get(k).strip() for k in _FIELD_KEYS):
            return None, [InteractionFieldError("values", "provide at least one of name/email/phone, or declined=true")]
        return validate_intake(fields, values)

    def fallback_directive(self, spec: dict[str, Any], reason: str) -> str:
        field_list = ", ".join(f.get("key", "") for f in spec.get("fields", []) if isinstance(f, dict) and f.get("key"))
        return (
            "This visitor's channel cannot display a form. Collect the requested details "
            f"({field_list}) conversationally: ask for ONE field at a time, in the order given, "
            f"naturally weaving in the reason ({reason}). When you have the values, call the "
            '`submit_interaction` tool with kind "identity_intake" and the values — it validates '
            "each field and will tell you if something looks wrong so you can re-ask. If the visitor "
            "declines to share, call `submit_interaction` with declined=true and continue helping "
            "them without the details."
        )

    async def host_effect(self, store: "SessionStore", session_id: str, values: dict[str, Any]) -> None:
        """Stamp the validated identity onto the session (metadata ``userName`` /
        ``contactEmail`` / ``contactPhone`` — the same keys the pre-chat/create path stashes and
        the OTP contact seam reads), so a captured contact is immediately OTP-verifiable. Only
        provided fields are written (an intake that collected just an email never clobbers a known
        name). Mirrors the Rust ``attach_interaction_effect`` → ``attach_session_identity``. Durable
        participant/CRM attach is a host concern."""
        await store.attach_session_identity(
            session_id,
            name=values.get("name") if isinstance(values.get("name"), str) else None,
            email=values.get("email") if isinstance(values.get("email"), str) else None,
            phone=values.get("phone") if isinstance(values.get("phone"), str) else None,
        )
