"""Per-user conversation scoping — the cross-user data-leak fix (th-8fe998).

Before this, ``list_conversations`` took no user filter and returned EVERY user's
conversations, and neither the resume path nor a sessionId lookup was owner-checked:
any authenticated user could enumerate and open anyone else's chats.

These cases are written from the ATTACKER's side — user A trying to see, resume, or
act on user B's conversations — plus the corners: an emailless/anonymous principal
(which owns nothing, so it reaches only OWNERLESS sessions — th-909995) and the
legitimately-unscoped auth-disabled flavor.

The scoping rule is Option B (th-909995): a session that HAS an owner is owner-checked;
a session with NO owner is open, as it was before scoping shipped. Fail-closing the
ownerless case instead denied anonymous and emailless principals their OWN sessions —
empty list, no resume, no send_message — which is why PR #309 reverted the .NET twin.

The sharpest case here is :func:`test_not_yours_is_byte_identical_to_never_existed`:
"someone else's id" and "an id that never existed" must produce the SAME bytes. Any
difference — a distinct error code, a reworded message, even a different branch that
happens to render the same today — is an existence oracle an attacker can use to
enumerate other users' conversation and session ids.
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from smooth_operator_server import dispatcher as dispatcher_module
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
        {"action": "get_conversation_messages"},
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
        {"action": "get_conversation_messages"},
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
async def test_get_conversation_messages_leaks_no_history_across_users() -> None:
    """The headline case (th-1b7ed0): ``get_conversation_messages`` skipped the
    chokepoint, so any authenticated user could read anyone's full history by
    sessionId. Assert on the PAYLOAD — an error code alone would still pass if the
    messages rode along beside it."""
    store = InMemorySessionStore()
    bob_session, _ = await _seed(store, BOB, "bob secret")

    alice = FrameDispatcher(store, None, access=_authed(ALICE))
    events = await _dispatch(
        alice, {"action": "get_conversation_messages", "requestId": "r1", "sessionId": bob_session}
    )

    assert [ev["type"] for ev in events] == ["error"]
    assert events[0]["error"]["code"] == "SESSION_NOT_FOUND"
    assert "bob secret" not in json.dumps(events)
    assert "messages" not in json.dumps(events)


@pytest.mark.asyncio
async def test_owner_can_read_their_own_conversation_messages() -> None:
    """The scoping guard must not break the legitimate read."""
    store = InMemorySessionStore()
    alice_session, _ = await _seed(store, ALICE, "mine")
    events = await _dispatch(
        FrameDispatcher(store, None, access=_authed(ALICE)),
        {"action": "get_conversation_messages", "requestId": "r1", "sessionId": alice_session},
    )
    assert events[0]["type"] == "immediate_response"
    assert [m["content"]["text"] for m in events[0]["data"]["messages"]] == ["mine"]


def test_visible_session_is_the_only_lookup() -> None:
    """Structural guard: ``_visible_session`` must be the ONLY place that turns a
    client-supplied sessionId into a session. A handler calling the store directly
    skips the ownership check — that is precisely how th-1b7ed0 shipped. Exactly one
    direct call site is expected: the one inside ``_visible_session`` itself."""
    source = Path(dispatcher_module.__file__).read_text()
    assert source.count("self._store.get_session(") == 1


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
async def test_emailless_principal_can_use_its_own_session(access: AccessContext) -> None:
    """Option B (th-909995). A principal with no email claim — an anonymous connection
    to an AUTH-ENABLED server, or an authenticated token carrying sub/org/role but no
    ``email`` — still owns the product: it can create a session, read it back, list its
    conversation, and send into it. Denying these (the th-8fe998 rule) locked such
    principals out of the sessions they had just created themselves."""
    store = InMemorySessionStore()
    dispatcher = FrameDispatcher(store, None, access=access)

    created = await _dispatch(dispatcher, {"action": "create_conversation_session", "requestId": "r1"})
    session_id = created[0]["data"]["sessionId"]
    conversation_id = created[0]["data"]["conversationId"]

    read = await _dispatch(dispatcher, {"action": "get_session", "requestId": "r1", "sessionId": session_id})
    assert read[0]["type"] == "immediate_response"
    assert read[0]["data"]["conversationId"] == conversation_id

    # send_message gets PAST the ownership gate: the only thing stopping it here is the
    # absent LLM gateway (this dispatcher has no chat client), NOT SESSION_NOT_FOUND.
    sent = await _dispatch(
        dispatcher, {"action": "send_message", "requestId": "r1", "sessionId": session_id, "message": "hi"}
    )
    assert sent[0]["error"]["code"] == "LLM_UNAVAILABLE"

    # And the conversation it created is listable by it.
    await store.append_message(conversation_id, MessageDirection.INBOUND, "hi")
    listed = await _dispatch(dispatcher, {"action": "list_conversations"})
    assert _conv_ids(listed) == [conversation_id]

    # Resume by conversationId binds back to the same conversation.
    resumed = await _dispatch(dispatcher, {"action": "create_conversation_session", "conversationId": conversation_id})
    assert resumed[0]["data"]["conversationId"] == conversation_id


@pytest.mark.asyncio
@pytest.mark.parametrize("access", [_authed(None), ANON_ENFORCED], ids=["emailless-principal", "anonymous-enforced"])
async def test_emailless_principal_still_cannot_reach_an_owned_session(access: AccessContext) -> None:
    """The permissive half of Option B is ONLY the ownerless case. An emailless scope
    matches no non-empty owner, so Alice's session and history stay unreachable."""
    store = InMemorySessionStore()
    alice_session, alice_conv = await _seed(store, ALICE, "alice secret")

    dispatcher = FrameDispatcher(store, None, access=access)
    for action in ("get_session", "get_conversation_messages"):
        events = await _dispatch(dispatcher, {"action": action, "requestId": "r1", "sessionId": alice_session})
        assert events[0]["error"]["code"] == "SESSION_NOT_FOUND"
        assert "alice secret" not in json.dumps(events)

    listed = await _dispatch(dispatcher, {"action": "list_conversations"})
    assert alice_conv not in _conv_ids(listed)

    # Nor by resuming her conversation id — that mints a fresh, empty one.
    resumed = await _dispatch(dispatcher, {"action": "create_conversation_session", "conversationId": alice_conv})
    assert resumed[0]["data"]["conversationId"] != alice_conv
    assert [m.text for m in await store.list_messages(alice_conv, 100)] == ["alice secret"]


@pytest.mark.asyncio
async def test_send_message_never_appends_to_another_users_session() -> None:
    """The reported P0: authenticated A writing into authenticated B's OWNED session.
    Assert on B's LOG, not just the error code — a refusal that still appended would
    pass a code-only check."""
    store = InMemorySessionStore()
    bob_session, bob_conv = await _seed(store, BOB, "bob secret")

    alice = FrameDispatcher(store, None, access=_authed(ALICE))
    events = await _dispatch(
        alice, {"action": "send_message", "requestId": "r1", "sessionId": bob_session, "message": "injected"}
    )
    assert [ev["type"] for ev in events] == ["error"]
    assert events[0]["error"]["code"] == "SESSION_NOT_FOUND"
    assert [m.text for m in await store.list_messages(bob_conv, 100)] == ["bob secret"]


@pytest.mark.asyncio
async def test_unowned_conversation_is_reachable_by_every_principal() -> None:
    """Option B's accepted trade-off: a conversation with NO owner (anonymous, emailless,
    or predating ownership) stays open, exactly as it was before scoping shipped. The
    alternative — keying anonymous scope on ``sub`` — pools every anonymous visitor
    under the literal sub "anonymous" and leaks their chats to each other."""
    store = InMemorySessionStore()
    orphan = await store.create_session("agent", None, None)
    await store.append_message(orphan.conversation_id, MessageDirection.INBOUND, "orphan")

    for access in (_authed(ALICE), _authed(BOB), _authed(None), ANON_ENFORCED):
        listed = await _dispatch(FrameDispatcher(store, None, access=access), {"action": "list_conversations"})
        assert _conv_ids(listed) == [orphan.conversation_id]


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

    assert [c.conversation_id for c in await store.list_conversations(BOB, enforced=True)] == []
    assert len(await store.list_conversations(ALICE, enforced=True)) == 1
    assert len(await store.list_conversations(None)) == 1  # unenforced = auth-disabled, unscoped


@pytest.mark.asyncio
async def test_store_enforced_scope_admits_ownerless_only_alongside_the_owner() -> None:
    """``enforced=True`` = "mine + nobody's"; ``enforced=False`` (auth disabled) = all."""
    store = InMemorySessionStore()
    _, alice_conv = await _seed(store, ALICE, "a")
    _, bob_conv = await _seed(store, BOB, "b")
    orphan = await store.create_session("agent", None, None)
    await store.append_message(orphan.conversation_id, MessageDirection.INBOUND, "o")

    def ids(summaries: list) -> set[str]:
        return {c.conversation_id for c in summaries}

    assert ids(await store.list_conversations(ALICE, enforced=True)) == {alice_conv, orphan.conversation_id}
    assert ids(await store.list_conversations(None, enforced=True)) == {orphan.conversation_id}
    assert ids(await store.list_conversations(None)) == {alice_conv, bob_conv, orphan.conversation_id}
