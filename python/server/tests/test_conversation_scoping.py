"""Per-user conversation scoping — the cross-user data-leak fix (th-8fe998).

Before this, ``list_conversations`` took no user filter and returned EVERY user's
conversations, and neither the resume path nor a sessionId lookup was owner-checked:
any authenticated user could enumerate and open anyone else's chats.

These cases are written from the ATTACKER's side — user A trying to see, resume, or
act on user B's conversations — plus the two fail-closed corners (authenticated but
emailless) and the one legitimately-unscoped corner (auth disabled).

The sharpest case here is :func:`test_not_yours_is_byte_identical_to_never_existed`:
"someone else's id" and "an id that never existed" must produce the SAME bytes. Any
difference — a distinct error code, a reworded message, even a different branch that
happens to render the same today — is an existence oracle an attacker can use to
enumerate other users' conversation and session ids.
"""

from __future__ import annotations

import json

import pytest

from smooth_operator_server.auth import AccessContext, Principal
from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.session_store import InMemorySessionStore, MessageDirection

ALICE = "alice@example.com"
BOB = "bob@example.com"


def _authed(email: str | None) -> AccessContext:
    """An auth-ENABLED context for a principal with (or without) an email claim."""
    return AccessContext(
        principal=Principal(sub=email or "no-email", org="acme", role="basic", email=email),
        is_anonymous=False,
    )


#: Auth ENABLED, but the connection presented no/invalid token.
ANON_ENFORCED: AccessContext = AccessContext.ANONYMOUS_ENFORCED  # type: ignore[attr-defined]
#: Auth DISABLED (local single-tenant flavor) — the only unscoped context.
AUTH_OFF: AccessContext = AccessContext.ANONYMOUS  # type: ignore[attr-defined]


async def _dispatch(dispatcher: FrameDispatcher, frame: dict) -> list[dict]:
    events: list[dict] = []
    await dispatcher.dispatch(json.dumps(frame), events.append)
    return events


async def _seed(store: InMemorySessionStore, owner: str, text: str) -> tuple[str, str]:
    """A conversation owned by ``owner`` with one inbound message. Returns
    ``(session_id, conversation_id)``."""
    session = await store.create_session("agent", None, None, owner_email=owner)
    await store.append_message(session.conversation_id, MessageDirection.INBOUND, text)
    return session.session_id, session.conversation_id


def _conv_ids(events: list[dict]) -> list[str]:
    return [c["conversationId"] for c in events[0]["data"]["conversations"]]


# --------------------------------------------------------------------------- #
# list_conversations — the leak itself
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
async def test_list_returns_only_the_callers_conversations() -> None:
    store = InMemorySessionStore()
    _, alice_conv = await _seed(store, ALICE, "alice secret")
    _, bob_conv = await _seed(store, BOB, "bob secret")

    alice_view = await _dispatch(FrameDispatcher(store, None, access=_authed(ALICE)), {"action": "list_conversations"})
    assert _conv_ids(alice_view) == [alice_conv]
    assert "bob secret" not in json.dumps(alice_view)

    bob_view = await _dispatch(FrameDispatcher(store, None, access=_authed(BOB)), {"action": "list_conversations"})
    assert _conv_ids(bob_view) == [bob_conv]


@pytest.mark.asyncio
async def test_scoping_is_applied_before_the_limit_not_after() -> None:
    """A filter applied AFTER the limit silently returns short/empty pages: 60 of Bob's
    conversations would fill the default page and push Alice's out entirely."""
    store = InMemorySessionStore()
    for i in range(60):
        await _seed(store, BOB, f"bob {i}")
    _, alice_conv = await _seed(store, ALICE, "alice")

    events = await _dispatch(FrameDispatcher(store, None, access=_authed(ALICE)), {"action": "list_conversations"})
    assert _conv_ids(events) == [alice_conv]

    # Same with an explicit small limit — the cap applies to Alice's already-scoped set.
    capped = await _dispatch(
        FrameDispatcher(store, None, access=_authed(ALICE)), {"action": "list_conversations", "limit": 1}
    )
    assert _conv_ids(capped) == [alice_conv]


@pytest.mark.asyncio
async def test_email_scope_ignores_case_and_whitespace() -> None:
    store = InMemorySessionStore()
    _, conv = await _seed(store, ALICE, "hi")
    events = await _dispatch(
        FrameDispatcher(store, None, access=_authed("  ALICE@Example.COM  ")), {"action": "list_conversations"}
    )
    assert _conv_ids(events) == [conv]


# --------------------------------------------------------------------------- #
# spoofing — the principal wins over the frame
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
async def test_client_supplied_email_cannot_claim_another_users_scope() -> None:
    store = InMemorySessionStore()
    _, bob_conv = await _seed(store, BOB, "bob secret")

    # Alice creates a session while claiming to be Bob in the frame.
    alice = FrameDispatcher(store, None, access=_authed(ALICE))
    created = await _dispatch(alice, {"action": "create_conversation_session", "userEmail": BOB, "userName": "Bob"})
    session_id = created[0]["data"]["sessionId"]
    stored = await store.get_session(session_id)
    assert stored is not None
    # The principal — not the frame — owns it, and drives the OTP contact too, so a
    # code can never be delivered to a client-chosen address.
    assert stored.owner_email == ALICE
    assert stored.contact_email == ALICE

    # And the claim buys no visibility into Bob's conversations.
    listed = await _dispatch(alice, {"action": "list_conversations"})
    assert bob_conv not in _conv_ids(listed)


@pytest.mark.asyncio
async def test_resuming_another_users_conversation_mints_a_fresh_one() -> None:
    store = InMemorySessionStore()
    _, bob_conv = await _seed(store, BOB, "bob secret")

    alice = FrameDispatcher(store, None, access=_authed(ALICE))
    events = await _dispatch(alice, {"action": "create_conversation_session", "conversationId": bob_conv})
    got = events[0]["data"]["conversationId"]
    assert got != bob_conv  # not bound to Bob's conversation

    # Bob's log is untouched, and Alice's new conversation is empty.
    assert [m.text for m in await store.list_messages(bob_conv, 100)] == ["bob secret"]
    assert await store.list_messages(got, 100) == []


@pytest.mark.asyncio
async def test_owner_can_still_resume_their_own_conversation() -> None:
    store = InMemorySessionStore()
    _, alice_conv = await _seed(store, ALICE, "mine")
    events = await _dispatch(
        FrameDispatcher(store, None, access=_authed(ALICE)),
        {"action": "create_conversation_session", "conversationId": alice_conv},
    )
    assert events[0]["data"]["conversationId"] == alice_conv
    assert [m.text for m in await store.list_messages(alice_conv, 100)] == ["mine"]


# --------------------------------------------------------------------------- #
# session-scoped actions — not-yours == not-found
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "frame",
    [
        {"action": "get_session"},
        {"action": "send_message", "message": "steal history"},
        {"action": "verify_otp", "code": "000000"},
    ],
)
async def test_session_actions_reject_another_users_session(frame: dict) -> None:
    store = InMemorySessionStore()
    bob_session, _ = await _seed(store, BOB, "bob secret")

    alice = FrameDispatcher(store, None, access=_authed(ALICE))
    events = await _dispatch(alice, {**frame, "requestId": "r1", "sessionId": bob_session})
    assert events[0]["type"] == "error"
    assert events[0]["error"]["code"] == "SESSION_NOT_FOUND"


@pytest.mark.asyncio
@pytest.mark.parametrize(
    "frame",
    [
        {"action": "get_session"},
        {"action": "send_message", "message": "steal history"},
        {"action": "verify_otp", "code": "000000"},
    ],
)
async def test_not_yours_is_byte_identical_to_never_existed(frame: dict) -> None:
    """The existence-oracle test. Probing someone else's session id must look EXACTLY
    like probing an id that was never issued — same events, same code, same message."""
    store = InMemorySessionStore()
    bob_session, _ = await _seed(store, BOB, "bob secret")
    alice = FrameDispatcher(store, None, access=_authed(ALICE))

    others = await _dispatch(alice, {**frame, "requestId": "r1", "sessionId": bob_session})
    ghost = await _dispatch(alice, {**frame, "requestId": "r1", "sessionId": "does-not-exist"})

    def normalized(events: list[dict]) -> str:
        # Blank the echoed session id (the caller's own input) and the wall-clock
        # timestamp; everything else must match exactly.
        scrubbed = [{k: v for k, v in ev.items() if k != "timestamp"} for ev in events]
        return json.dumps(scrubbed).replace(bob_session, "<ID>").replace("does-not-exist", "<ID>")

    assert normalized(others) == normalized(ghost)


@pytest.mark.asyncio
async def test_owner_can_use_their_own_session() -> None:
    store = InMemorySessionStore()
    alice_session, alice_conv = await _seed(store, ALICE, "mine")
    events = await _dispatch(
        FrameDispatcher(store, None, access=_authed(ALICE)),
        {"action": "get_session", "requestId": "r1", "sessionId": alice_session},
    )
    assert events[0]["type"] == "immediate_response"
    assert events[0]["data"]["conversationId"] == alice_conv


# --------------------------------------------------------------------------- #
# fail-closed corners
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
@pytest.mark.parametrize("access", [_authed(None), ANON_ENFORCED], ids=["emailless-principal", "anonymous-enforced"])
async def test_auth_enabled_without_an_email_sees_nothing(access: AccessContext) -> None:
    """Fail CLOSED: a principal we can't scope gets an empty list and no session —
    never a silent fallback to unscoped."""
    store = InMemorySessionStore()
    alice_session, alice_conv = await _seed(store, ALICE, "alice secret")
    unowned_session = (await store.create_session("agent", None, None)).session_id

    dispatcher = FrameDispatcher(store, None, access=access)
    listed = await _dispatch(dispatcher, {"action": "list_conversations"})
    assert listed[0]["data"]["conversations"] == []
    assert alice_conv not in json.dumps(listed)

    for session_id in (alice_session, unowned_session):
        events = await _dispatch(dispatcher, {"action": "get_session", "requestId": "r1", "sessionId": session_id})
        assert events[0]["error"]["code"] == "SESSION_NOT_FOUND"


@pytest.mark.asyncio
async def test_unowned_conversation_is_invisible_to_every_principal() -> None:
    """A session minted with no owner (e.g. by a host that forgot to pass one) belongs
    to nobody — the default is invisible, not public."""
    store = InMemorySessionStore()
    orphan = await store.create_session("agent", None, None)
    await store.append_message(orphan.conversation_id, MessageDirection.INBOUND, "orphan")

    for access in (_authed(ALICE), _authed(BOB)):
        listed = await _dispatch(FrameDispatcher(store, None, access=access), {"action": "list_conversations"})
        assert listed[0]["data"]["conversations"] == []


@pytest.mark.asyncio
async def test_auth_disabled_stays_unscoped() -> None:
    """The ONE unscoped path: no auth configured (local/dev single tenant). Everything
    is listable and resumable exactly as before this fix."""
    store = InMemorySessionStore()
    _, a_conv = await _seed(store, ALICE, "a")
    _, b_conv = await _seed(store, BOB, "b")
    orphan = await store.create_session("agent", None, None)
    await store.append_message(orphan.conversation_id, MessageDirection.INBOUND, "o")

    local = FrameDispatcher(store, None, access=AUTH_OFF)
    listed = await _dispatch(local, {"action": "list_conversations"})
    assert set(_conv_ids(listed)) == {a_conv, b_conv, orphan.conversation_id}

    # Resume works regardless of owner, and the client-supplied email is still the
    # OTP contact (no principal exists to override it).
    resumed = await _dispatch(local, {"action": "create_conversation_session", "conversationId": a_conv})
    assert resumed[0]["data"]["conversationId"] == a_conv
    created = await _dispatch(local, {"action": "create_conversation_session", "userEmail": ALICE})
    stored = await store.get_session(created[0]["data"]["sessionId"])
    assert stored is not None and stored.contact_email == ALICE


# --------------------------------------------------------------------------- #
# store-level guards
# --------------------------------------------------------------------------- #


@pytest.mark.asyncio
async def test_store_list_conversations_requires_an_explicit_scope() -> None:
    """The parameter is REQUIRED, not defaulted: an optional one would be fail-OPEN and
    would let a downstream store implementation ship unscoped without noticing."""
    store = InMemorySessionStore()
    await _seed(store, ALICE, "a")
    with pytest.raises(TypeError):
        await store.list_conversations()  # type: ignore[call-arg]

    assert [c.conversation_id for c in await store.list_conversations(BOB)] == []
    assert len(await store.list_conversations(ALICE)) == 1
    assert len(await store.list_conversations(None)) == 1  # None = auth-disabled, unscoped
