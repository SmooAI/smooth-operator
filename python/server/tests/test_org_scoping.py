"""Org is the OUTER scope, applied before ownership.

The gap this closes: ``_visible_session`` returned the session for ANY ownerless
conversation (deliberate — th-909995 keeps anonymous / emailless / legacy sessions
reachable) and never consulted org. So an ownerless conversation belonging to
another org was readable by anyone holding its id — authorization resting on an
unguessable UUID, which leaks through logs, referrers and screenshots.
"""

from __future__ import annotations

import json
from dataclasses import replace

import pytest

from smooth_operator_server.auth import AccessContext, Principal
from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.session_store import InMemorySessionStore


def _authed(org: str, email: str | None = None) -> AccessContext:
    """An auth-ENABLED principal in ``org``, optionally with no email claim."""
    return AccessContext(
        principal=Principal(sub=email or "no-email", org=org, role="basic", email=email),
        is_anonymous=False,
    )


async def _dispatch(dispatcher: FrameDispatcher, frame: dict) -> list[dict]:
    events: list[dict] = []
    await dispatcher.dispatch(json.dumps(frame), events.append)
    return events


@pytest.mark.asyncio
async def test_another_org_cannot_reach_an_ownerless_session() -> None:
    """The actual gap: an OWNERLESS session, which ownership can never block — so only
    the org check can. An owned session here would pass with the org check removed and
    prove nothing."""
    store = InMemorySessionStore()
    # Emailless principal in org-a → an ownerless conversation.
    session = await store.create_session("agent", None, None, owner_email=None, org_id="org-a")

    # Its own org can still reach it — th-909995 intact.
    same_org = await _dispatch(
        FrameDispatcher(store, None, access=_authed("org-a")),
        {"action": "get_session", "requestId": "r1", "sessionId": session.session_id},
    )
    assert same_org[0]["type"] == "immediate_response"

    # Another org must not, and must be told the same thing an unknown id would get.
    other_org = await _dispatch(
        FrameDispatcher(store, None, access=_authed("org-b")),
        {"action": "get_session", "requestId": "r2", "sessionId": session.session_id},
    )
    assert other_org[0]["type"] == "error"
    assert other_org[0]["data"]["error"]["code"] == "SESSION_NOT_FOUND"

    unknown = await _dispatch(
        FrameDispatcher(store, None, access=_authed("org-b")),
        {"action": "get_session", "requestId": "r3", "sessionId": "00000000-0000-4000-8000-000000000000"},  # noqa: E501
    )
    # Both messages quote the id the CALLER supplied, which they already know — so the
    # payloads differ only there. What must not vary is the shape: same code, same
    # template. Any extra signal (a different code, a "forbidden" wording) would let an
    # attacker diff the two responses to learn which ids are real.
    unknown_id = "00000000-0000-4000-8000-000000000000"
    assert other_org[0]["data"]["error"] == {
        "code": "SESSION_NOT_FOUND",
        "message": f"session '{session.session_id}' not found",
    }
    assert unknown[0]["data"]["error"] == {
        "code": "SESSION_NOT_FOUND",
        "message": f"session '{unknown_id}' not found",
    }


@pytest.mark.asyncio
async def test_another_org_cannot_resume_an_ownerless_conversation() -> None:
    store = InMemorySessionStore()
    session = await store.create_session("agent", None, None, owner_email=None, org_id="org-a")

    # Same org resumes it.
    same = await store.create_session(
        "agent", None, None, session.conversation_id, owner_email=None, enforced=True, org_id="org-a"
    )
    assert same.conversation_id == session.conversation_id

    # Another org gets a FRESH conversation instead.
    other = await store.create_session(
        "agent", None, None, session.conversation_id, owner_email=None, enforced=True, org_id="org-b"
    )
    assert other.conversation_id != session.conversation_id


@pytest.mark.asyncio
async def test_unrecorded_org_falls_through_to_ownership() -> None:
    """Rows created before org capture carry no org; denying them would lock people out
    of conversations they already own."""
    store = InMemorySessionStore()
    session = await store.create_session("agent", None, None, owner_email=None)
    # A row that predates org capture: StoredSession is frozen, so replace it.
    session = replace(session, owner_org=None)
    store._sessions[session.session_id] = session  # noqa: SLF001

    events = await _dispatch(
        FrameDispatcher(store, None, access=_authed("org-a")),
        {"action": "get_session", "requestId": "r1", "sessionId": session.session_id},
    )
    assert events[0]["type"] == "immediate_response"
