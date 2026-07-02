"""Session identity-verified bit + OTP contact capture on the in-memory store.

Mirrors the Rust ``state.rs`` tests (``session_authenticated_round_trips_and_defaults_false``
and ``session_contact_reads_stashed_email``).
"""

from __future__ import annotations

import pytest

from smooth_operator_server.session_store import InMemorySessionStore


@pytest.mark.asyncio
async def test_session_authenticated_round_trips_and_defaults_false() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent", None, None)
    sid = session.session_id

    assert await store.is_session_authenticated(sid) is False  # fresh: unverified
    assert await store.is_session_authenticated("missing") is False  # unknown: unverified

    await store.set_session_authenticated(sid, True)
    assert await store.is_session_authenticated(sid) is True

    await store.set_session_authenticated(sid, False)
    assert await store.is_session_authenticated(sid) is False


@pytest.mark.asyncio
async def test_set_authenticated_is_noop_for_unknown_session() -> None:
    # A code can't authenticate a session the store doesn't hold.
    store = InMemorySessionStore()
    await store.set_session_authenticated("missing", True)
    assert await store.is_session_authenticated("missing") is False


@pytest.mark.asyncio
async def test_contact_email_captured_and_trimmed() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent", "Alice", "  alice@example.com  ")
    assert session.contact_email == "alice@example.com"

    # No email / blank email → no contact (the server then can't offer OTP).
    assert (await store.create_session("agent", None, None)).contact_email is None
    assert (await store.create_session("agent", None, "   ")).contact_email is None
