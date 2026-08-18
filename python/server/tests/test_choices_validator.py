"""Unit tests for the ``choices`` Rich Interaction kind's validator + parser.

Mirrors the Rust reference tests in ``rust/smooth-operator/src/choices.rs``, and
additionally validates the shared conformance fixtures
(``spec/conformance/fixtures.json``) so the Python validator agrees with the golden
spec every language checks against.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from smooth_operator_server.choices import ChoicesKind, parse_questions, validate_choices

_SPEC = Path(__file__).resolve().parents[3] / "spec" / "conformance" / "fixtures.json"


@pytest.fixture(scope="module")
def fixtures() -> dict:
    return json.loads(_SPEC.read_text())


def _q(header: str, labels: list[str], multi: bool = False) -> dict:
    return {
        "question": f"{header}?",
        "header": header,
        "options": [{"label": label, "description": ""} for label in labels],
        **({"multiSelect": True} if multi else {}),
    }


def _a(header: str, options: list[str], other: str | None = None) -> dict:
    ans: dict = {"header": header, "options": options}
    if other is not None:
        ans["other"] = other
    return ans


def test_valid_single_select_normalizes() -> None:
    canonical, errors = validate_choices([_q("Plan", ["Basic", "Pro"])], {"answers": [_a("Plan", ["  Pro  "])]})
    assert errors == []
    assert canonical == {"answers": [{"header": "Plan", "options": ["Pro"]}]}


def test_valid_multi_select_keeps_all_picks() -> None:
    canonical, errors = validate_choices(
        [_q("Topics", ["Sales", "Support", "Billing"], multi=True)],
        {"answers": [_a("Topics", ["Sales", "Billing"])]},
    )
    assert errors == []
    assert canonical["answers"][0]["options"] == ["Sales", "Billing"]


def test_other_escape_hatch_is_accepted() -> None:
    canonical, errors = validate_choices(
        [_q("Plan", ["Basic", "Pro"])], {"answers": [_a("Plan", [], other="  Enterprise, actually  ")]}
    )
    assert errors == []
    assert canonical["answers"][0]["options"] == []
    assert canonical["answers"][0]["other"] == "Enterprise, actually"


def test_unknown_label_is_a_field_error() -> None:
    canonical, errors = validate_choices([_q("Plan", ["Basic", "Pro"])], {"answers": [_a("Plan", ["Platinum"])]})
    assert canonical is None
    assert len(errors) == 1
    assert errors[0].field == "Plan"
    assert "not one of the offered" in errors[0].message


def test_single_select_rejects_multiple_picks() -> None:
    _, errors = validate_choices([_q("Plan", ["Basic", "Pro"])], {"answers": [_a("Plan", ["Basic", "Pro"])]})
    assert any("single answer" in e.message for e in errors)


def test_unanswered_question_is_required() -> None:
    _, errors = validate_choices(
        [_q("Plan", ["Basic", "Pro"]), _q("Size", ["S", "M"])], {"answers": [_a("Plan", ["Pro"])]}
    )
    assert len(errors) == 1
    assert errors[0].field == "Size"
    assert "must be answered" in errors[0].message


def test_empty_answer_needs_a_pick_or_other() -> None:
    _, errors = validate_choices([_q("Plan", ["Basic", "Pro"])], {"answers": [_a("Plan", [])]})
    assert any("select an option" in e.message for e in errors)


def test_format_only_when_spec_is_gone() -> None:
    # No questions (prior-turn fallback raise) → membership can't be checked; any answer
    # with a pick is accepted as-is.
    canonical, errors = validate_choices([], {"answers": [_a("Plan", ["Anything"])]})
    assert errors == []
    assert canonical["answers"][0]["options"] == ["Anything"]
    # …but a pickless answer still fails.
    _, errors2 = validate_choices([], {"answers": [_a("Plan", [])]})
    assert any("select an option" in e.message for e in errors2)


def test_parse_questions_enforces_the_contract() -> None:
    # Happy path with shorthand string options.
    qs = parse_questions([{"question": "Which plan?", "header": "Plan", "options": ["Basic", "Pro"]}])
    assert len(qs) == 1
    assert qs[0]["options"][0]["label"] == "Basic"
    assert "multiSelect" not in qs[0]

    with pytest.raises(ValueError):  # too many questions
        parse_questions([{"question": "q", "header": f"H{i}", "options": ["a", "b"]} for i in range(5)])
    with pytest.raises(ValueError):  # too few options
        parse_questions([{"question": "q", "header": "H", "options": ["only"]}])
    with pytest.raises(ValueError):  # header too long
        parse_questions([{"question": "q", "header": "ThisHeaderIsWayTooLong", "options": ["a", "b"]}])
    with pytest.raises(ValueError):  # duplicate headers
        parse_questions(
            [
                {"question": "q1", "header": "H", "options": ["a", "b"]},
                {"question": "q2", "header": "H", "options": ["a", "b"]},
            ]
        )


def test_kind_wires_the_reference_surface() -> None:
    kind = ChoicesKind()
    assert kind.kind() == "choices"
    assert kind.capability() == "choice_chips"
    assert kind.tool_schema()["name"] == "request_choices"

    req = kind.parse_request(
        {
            "questions": [
                {
                    "question": "Which plan interests you?",
                    "header": "Plan",
                    "options": [{"label": "Basic"}, {"label": "Pro"}],
                }
            ],
            "reason": "to route you",
        }
    )
    assert req.kind == "choices"
    assert req.reason == "to route you"
    assert req.spec["questions"][0]["header"] == "Plan"

    canonical, errors = kind.validate(req.spec, {"answers": [{"header": "Plan", "options": ["Pro"]}]})
    assert errors == []
    assert canonical["answers"][0]["options"][0] == "Pro"

    directive = kind.fallback_directive(req.spec, "to route you")
    assert "Basic, Pro" in directive
    assert "submit_interaction" in directive


def test_shared_fixtures_validate_to_the_canonical_payload(fixtures: dict) -> None:
    """The shared ``choices`` fixtures: the golden spec + values must validate to exactly
    the golden payload's values (the same golden shapes the Rust/Go/TS/C# engines check)."""
    spec = fixtures["choices_spec"]["instance"]
    values = fixtures["choices_values"]["instance"]
    expected = fixtures["choices_payload"]["instance"]["values"]

    canonical, errors = ChoicesKind().validate(spec, values)
    assert errors == []
    assert canonical == expected
