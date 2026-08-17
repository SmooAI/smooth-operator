"""The env-parity contract: every server implementation reads the canonical
``SMOOTH_AGENT_*`` names, and each one's pre-parity name keeps working as an alias
so no existing deployment breaks. Canonical wins when both are set."""

from __future__ import annotations

import pytest

from smooth_operator_server.__main__ import resolve_addr, resolve_seed_kb


@pytest.mark.parametrize(
    ("env", "expected"),
    [
        ({}, "127.0.0.1:8787"),
        ({"SMOOTH_AGENT_BIND": "0.0.0.0", "SMOOTH_AGENT_PORT": "9000"}, "0.0.0.0:9000"),
        ({"SMOOTH_OPERATOR_BIND": "0.0.0.0:8793"}, "0.0.0.0:8793"),
        (
            {"SMOOTH_OPERATOR_BIND": "10.0.0.1:8793", "SMOOTH_AGENT_BIND": "0.0.0.0", "SMOOTH_AGENT_PORT": "9000"},
            "0.0.0.0:9000",
        ),
        # A canonical host with no canonical port leaves the alias's port in place, so
        # half-migrated deployments keep the port they were already serving on.
        ({"SMOOTH_OPERATOR_BIND": "10.0.0.1:8793", "SMOOTH_AGENT_BIND": "0.0.0.0"}, "0.0.0.0:8793"),
        # Documented as a bare host, but accepting host:port costs nothing and is what
        # someone migrating off the combined name types.
        ({"SMOOTH_AGENT_BIND": "0.0.0.0:9100"}, "0.0.0.0:9100"),
        ({"SMOOTH_AGENT_BIND": "  "}, "127.0.0.1:8787"),
    ],
)
def test_resolve_addr(env: dict[str, str], expected: str) -> None:
    assert resolve_addr(env) == expected


@pytest.mark.parametrize(
    ("env", "expected"),
    [
        ({}, False),
        ({"SMOOTH_AGENT_SEED_KB": "1"}, True),
        ({"SMOOTH_OPERATOR_SEED_KB": "1"}, True),
        # Canonical wins in BOTH directions — an explicit "0" must be able to turn off
        # what the alias turns on, which an `or`-chain would silently get wrong.
        ({"SMOOTH_AGENT_SEED_KB": "0", "SMOOTH_OPERATOR_SEED_KB": "1"}, False),
        ({"SMOOTH_AGENT_SEED_KB": "1", "SMOOTH_OPERATOR_SEED_KB": "0"}, True),
    ],
)
def test_resolve_seed_kb(env: dict[str, str], expected: bool) -> None:
    assert resolve_seed_kb(env) is expected
