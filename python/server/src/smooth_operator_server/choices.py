"""Choices — a structured multiple-choice ask (modeled on Claude Code's
``AskUserQuestion``): the reference **Rich Interaction kind** (see :mod:`interaction`
and the Rust reference ``rust/smooth-operator/src/choices.rs``, mirrored exactly).

The agent asks 1–4 short questions, each with 2–4 labeled options; the turn parks until
the visitor picks. Every question also carries an implicit free-text **"Other"** escape
hatch, so the visitor can answer outside the enumerated options (exactly as
``AskUserQuestion`` always offers "Other").

- On a channel that declared the ``choice_chips`` capability, the ``request_choices``
  tool parks the turn and the server emits ``interaction_required { kind: "choices" }``;
  the client's chip/menu card resumes with a ``submit_interaction`` action.
- On a **text-only** channel the same raise degrades to a conversational directive that
  enumerates the questions + options, and the model submits the picks through the generic
  ``submit_interaction`` *tool*.

Both paths validate through :func:`validate_choices` — one implementation, one behavior —
and resume the turn with the same structured payload.
"""

from __future__ import annotations

from typing import Any

from .interaction import InteractionFieldError, InteractionKind, InteractionRequest

#: Max length of a question's short ``header`` label (chip/tab caption).
HEADER_MAX_CHARS = 12


def _selection_count(answer: dict[str, Any]) -> int:
    """Total picks in a normalized answer: selected labels + one for a non-blank
    "Other"."""
    return len(answer.get("options", [])) + (1 if answer.get("other") else 0)


def _normalize_answer(raw: dict[str, Any]) -> dict[str, Any]:
    """Trim the header + labels, drop blank labels, and collapse a blank/whitespace
    "Other" to absent (mirrors the Rust normalization pass)."""
    header = str(raw.get("header", "")).strip()
    options = [o.strip() for o in raw.get("options", []) if isinstance(o, str) and o.strip()]
    other_raw = raw.get("other")
    other = other_raw.strip() if isinstance(other_raw, str) and other_raw.strip() else None
    normalized: dict[str, Any] = {"header": header, "options": options}
    if other is not None:
        normalized["other"] = other
    return normalized


def validate_choices(
    questions: list[dict[str, Any]], values: dict[str, Any]
) -> tuple[dict[str, Any] | None, list[InteractionFieldError]]:
    """Validate submitted ``values`` against the raised ``questions``, returning
    ``(normalized_values, [])`` or ``(None, errors)`` with every per-question failure.

    Rules (mirrors ``choices.rs`` byte-for-byte):
    - **every** question must be answered (a selection or a non-blank "Other");
    - each selected label must be one of that question's option labels;
    - single-select (``multiSelect: false``): exactly one pick (one label XOR "Other");
      multi-select: one or more picks (labels and/or "Other");
    - a blank/whitespace "Other" is treated as absent; labels are trimmed.

    When ``questions`` is empty (a prior-turn fallback raise whose spec is gone),
    validation degrades to **format-only**: labels can't be checked for membership, so any
    answer with at least one pick is accepted as-is."""
    normalized = [_normalize_answer(a) for a in values.get("answers", []) if isinstance(a, dict)]

    # Format-only path: no spec to check membership/required-ness against.
    if not questions:
        errors: list[InteractionFieldError] = []
        for answer in normalized:
            if _selection_count(answer) == 0:
                errors.append(InteractionFieldError(answer["header"], "select an option or provide an 'other' answer"))
        if not normalized:
            errors.append(InteractionFieldError("answers", "provide an answer for each question, or declined=true"))
        if errors:
            return None, errors
        return {"answers": normalized}, []

    errors = []
    out: list[dict[str, Any]] = []
    for question in questions:
        header = question.get("header", "")
        answer = next((a for a in normalized if a["header"] == header), None)
        if answer is None:
            errors.append(InteractionFieldError(header, "this question must be answered"))
            continue

        # Every selected label must be one of the enumerated options.
        option_labels = [o.get("label") for o in question.get("options", []) if isinstance(o, dict)]
        bad_label = False
        for label in answer["options"]:
            if label not in option_labels:
                bad_label = True
                errors.append(InteractionFieldError(header, f"'{label}' is not one of the offered options"))

        count = _selection_count(answer)
        if count == 0:
            errors.append(InteractionFieldError(header, "select an option or provide an 'other' answer"))
        elif not question.get("multiSelect", False) and count > 1:
            errors.append(InteractionFieldError(header, "this question takes a single answer"))

        if not bad_label:
            out.append(answer)

    if errors:
        return None, errors
    return {"answers": out}, []


def parse_questions(raw: Any) -> list[dict[str, Any]]:
    """Parse the raise tool's ``questions`` argument into validated question dicts.

    Enforces the LLM-facing contract so the model produces usable cards: 1–4 questions,
    each with a non-empty prompt, a non-empty header ≤12 chars (unique within the raise),
    and 2–4 options with non-empty labels. Raises :class:`ValueError` on any violation
    (the engine surfaces the text to the model). Mirrors ``choices.rs::parse_questions``,
    including the shorthand where a bare string is accepted as an option label."""
    if not isinstance(raw, list):
        raise ValueError("'questions' must be an array")
    if not 1 <= len(raw) <= 4:
        raise ValueError("'questions' must contain between 1 and 4 questions")
    questions: list[dict[str, Any]] = []
    seen_headers: list[str] = []
    for item in raw:
        if not isinstance(item, dict):
            raise ValueError("each question must be an object")
        question = str(item.get("question") or "").strip()
        if not question:
            raise ValueError("each question needs a non-empty 'question'")
        header = str(item.get("header") or "").strip()
        if not header:
            raise ValueError("each question needs a non-empty 'header'")
        if len(header) > HEADER_MAX_CHARS:
            raise ValueError(f"header '{header}' is too long (max {HEADER_MAX_CHARS} characters)")
        if header in seen_headers:
            raise ValueError(f"duplicate question header '{header}'")
        seen_headers.append(header)

        raw_options = item.get("options")
        if not isinstance(raw_options, list):
            raise ValueError(f"question '{header}' needs an 'options' array")
        if not 2 <= len(raw_options) <= 4:
            raise ValueError(f"question '{header}' must offer between 2 and 4 options")
        options: list[dict[str, str]] = []
        for opt in raw_options:
            # Accept the object form `{ label, description? }` and the shorthand bare
            # string the model sometimes emits.
            if isinstance(opt, str):
                option = {"label": opt.strip(), "description": ""}
            elif isinstance(opt, dict):
                option = {
                    "label": str(opt.get("label") or "").strip(),
                    "description": str(opt.get("description") or "").strip(),
                }
            else:
                raise ValueError(f"invalid option entry in '{header}': {opt!r}")
            if not option["label"]:
                raise ValueError(f"an option in '{header}' has an empty label")
            options.append(option)

        parsed: dict[str, Any] = {"question": question, "header": header, "options": options}
        if item.get("multiSelect") is True:
            parsed["multiSelect"] = True
        questions.append(parsed)
    return questions


class ChoicesKind(InteractionKind):
    """The ``choices`` Rich Interaction kind — a structured multiple-choice ask modeled on
    ``AskUserQuestion`` (see the module docs and
    ``spec/interactions/choices.schema.json``)."""

    def kind(self) -> str:
        return "choices"

    def capability(self) -> str:
        return "choice_chips"

    def tool_schema(self) -> dict[str, Any]:
        return {
            "name": "request_choices",
            "description": (
                "Ask the visitor a structured multiple-choice question (1–4 questions, each with "
                "2–4 labeled options) and wait for their pick. On channels that can render "
                "chips/menus the visitor taps an option; on text channels you will be told to "
                "enumerate the options and accept a natural-language answer. An implicit free-text "
                '"Other" is always available, so use this whenever the answer is likely (but not '
                "certainly) one of a small set — never free-form the menu yourself."
            ),
            "parameters": {
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 4,
                        "description": "The questions to ask, in order (1–4).",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {
                                    "type": "string",
                                    "description": "The question prompt shown to the visitor.",
                                },
                                "header": {
                                    "type": "string",
                                    "maxLength": HEADER_MAX_CHARS,
                                    "description": "A short label (≤12 chars), unique within the raise. Used as the answer key and the chip/tab caption.",
                                },
                                "options": {
                                    "type": "array",
                                    "minItems": 2,
                                    "maxItems": 4,
                                    "description": "The 2–4 options to offer. A free-text 'Other' is always available in addition.",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {
                                                "type": "string",
                                                "description": "The option label (the value submitted).",
                                            },
                                            "description": {
                                                "type": "string",
                                                "description": "A short gloss for the option.",
                                            },
                                        },
                                        "required": ["label"],
                                    },
                                },
                                "multiSelect": {
                                    "type": "boolean",
                                    "description": "Allow selecting more than one option (default false).",
                                },
                            },
                            "required": ["question", "header", "options"],
                        },
                    },
                    "reason": {
                        "type": "string",
                        "description": 'Why you\'re asking, phrased for the visitor (e.g. "to route you to the right team").',
                    },
                },
                "required": ["questions", "reason"],
            },
        }

    def parse_request(self, args: dict[str, Any]) -> InteractionRequest:
        questions = parse_questions(args.get("questions"))
        reason = str(args.get("reason") or "").strip() or "to help you better"
        return InteractionRequest(kind=self.kind(), spec={"questions": questions}, reason=reason)

    def validate(
        self, spec: dict[str, Any] | None, values: dict[str, Any]
    ) -> tuple[dict[str, Any] | None, list[InteractionFieldError]]:
        questions = (spec or {}).get("questions") or []
        if not isinstance(values, dict):
            return None, [InteractionFieldError("values", "invalid values shape: expected an object")]
        return validate_choices(questions, values)

    def fallback_directive(self, spec: dict[str, Any], reason: str) -> str:
        lines = []
        for q in spec.get("questions", []):
            question = q.get("question")
            if not question:
                continue
            header = q.get("header") or question
            multi = q.get("multiSelect", False)
            opts = ", ".join(o.get("label", "") for o in q.get("options", []) if isinstance(o, dict))
            suffix = " (choose one or more)" if multi else ""
            lines.append(f"- [{header}] {question} Options: {opts}{suffix}.")
        enumerated = "\n".join(lines)
        return (
            "This visitor's channel cannot display choice chips. Ask the following question(s) "
            f"conversationally, naturally weaving in the reason ({reason}), and read out each option "
            f"so the visitor can pick:\n{enumerated}\nThe visitor may also answer with something not "
            "listed (that's fine — capture it as their 'other' answer). When you have their pick(s), "
            'call the `submit_interaction` tool with kind "choices" and `values.answers` — one entry '
            'per question `{ header, options: [chosen label(s)], other?: "their free-text answer" }`. '
            "It validates each answer and will tell you if a pick isn't offered so you can re-ask. If "
            "the visitor declines to choose, call `submit_interaction` with declined=true and continue "
            "helping them."
        )
