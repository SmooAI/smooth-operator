"""``list_conversations`` paging + search (SMOODEV-3744).

The Python port of ``rust/smooth-operator-server/tests/list_conversations_paging.rs``
plus the cursor/search unit tests at the bottom of ``rust/smooth-operator/src/adapter.rs``.
Test names follow their Rust counterparts.

The sidebar used to return only the newest ``limit`` rows with no way to reach older
ones, and a client-side search could only filter what it had loaded. These pin the
keyset ``cursor`` / ``nextCursor`` and the server-side ``query``:

- pages are disjoint and, together, exactly the unpaged listing;
- ties on ``updatedAt`` are broken by id, so equal timestamps neither repeat nor
  vanish across a page boundary;
- a conversation bumped mid-paging is never returned twice, and every untouched one is
  still returned (the bumped one moves above the cursor — documented keyset behaviour);
- a search narrows the caller's scope and never widens it: another member's matching
  chat is still excluded.

Python scoping differs from the Rust owned fast path in one way: an authenticated
principal's listing is "mine + ownerless" (th-909995), the same as the Rust generic
scan. So the Rust ``owned_*`` tests run here on a world with no ownerless row, and the
``scan_*`` tests on one that has it.

The scenario drivers (``check_*``) take any :class:`SessionStore` plus a timestamp
setter, so ``test_postgres_store.py`` runs the very same assertions against Postgres.
"""

from __future__ import annotations

import asyncio
import base64
import json
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from datetime import datetime, timedelta, timezone

import pytest

from smooth_operator_server.auth import AccessContext, Principal
from smooth_operator_server.dispatcher import FrameDispatcher
from smooth_operator_server.session_store import (
    ConversationKey,
    ConversationSummaryQuery,
    InMemorySessionStore,
    MessageDirection,
    SessionStore,
)

ORG = "org-alpha"
ME = "brent@smoo.ai"
TARA = "tara@smoo.ai"

#: Seeds sit an hour in the past (with a microsecond part) so a real append — the
#: "bump" — is always newer than every seeded row.
_BASE = datetime.now(timezone.utc).replace(microsecond=123456) - timedelta(hours=1)

SetUpdatedAt = Callable[[str, datetime], Awaitable[None]]


def ago(secs: int) -> datetime:
    return _BASE - timedelta(seconds=secs)


def authed(email: str | None, org: str = ORG) -> AccessContext:
    """An auth-ENABLED context (scoped listing) for a principal in ``org``."""
    return AccessContext(
        principal=Principal(sub=email or "no-email", org=org, role="basic", email=email), is_anonymous=False
    )


async def seed(store: SessionStore, set_ts: SetUpdatedAt, ts: datetime, owner: str | None, first: str, org: str) -> str:
    """A non-empty conversation updated at ``ts``, owned by ``owner`` (None = the
    ownerless widget/SMS case), whose first inbound message is ``first``."""
    session = await store.create_session("agent", None, None, owner_email=owner, enforced=True, org_id=org)
    await store.append_message(session.conversation_id, MessageDirection.INBOUND, first)
    await store.append_message(session.conversation_id, MessageDirection.OUTBOUND, "on it")
    await set_ts(session.conversation_id, ts)
    return session.conversation_id


def in_memory_setter(store: InMemorySessionStore) -> SetUpdatedAt:
    async def set_ts(conversation_id: str, ts: datetime) -> None:
        store._updated_at[conversation_id] = ts  # noqa: SLF001 — test seam: deterministic timestamps

    return set_ts


@dataclass
class World:
    store: SessionStore
    org: str
    #: Visible to ME, newest first (mine, plus the ownerless row when present).
    visible: list[str]
    #: Mine only, newest first.
    mine: list[str]
    #: The one of mine whose first message mentions Acme.
    acme: str
    theirs: str
    machine: str | None
    #: My chat in ANOTHER org that matches every search — seeded only for stores that
    #: scope listings by org (Postgres). The in-memory store is single-tenant and lists
    #: across orgs by design (see session_store.InMemorySessionStore.list_conversations).
    foreign: str | None


async def build_world(
    store: SessionStore, set_ts: SetUpdatedAt, *, org: str = ORG, with_machine: bool, org_scoped: bool = False
) -> World:
    mine: list[tuple[datetime, str]] = []
    acme = ""
    # Seven of mine; three share one timestamp to force the id tiebreak across a page
    # boundary at limit 3 (and at limit 2 once the ownerless row is in the listing).
    tied = ago(400)
    for i, ts in enumerate([ago(10), ago(20), tied, tied, tied, ago(500), ago(600)]):
        first = "Where is the Acme invoice?" if i == 5 else "status update"
        conv = await seed(store, set_ts, ts, ME, first, org)
        if i == 5:
            acme = conv
        mine.append((ts, conv))
    theirs = await seed(store, set_ts, ago(15), TARA, "acme renewal", org)
    # Another org's chat, mine by email, that matches every search below: org is the
    # OUTER scope, so it must never show.
    foreign = await seed(store, set_ts, ago(5), ME, "acme status update", f"{org}-foreign") if org_scoped else None
    visible = list(mine)
    machine = None
    if with_machine:
        machine = await seed(store, set_ts, ago(25), None, "ACME widget chat", org)
        visible.append((ago(25), machine))

    def newest_first(rows: list[tuple[datetime, str]]) -> list[str]:
        return [conv for _, conv in sorted(rows, reverse=True)]

    return World(
        store=store,
        org=org,
        visible=newest_first(visible),
        mine=newest_first(mine),
        acme=acme,
        theirs=theirs,
        machine=machine,
        foreign=foreign,
    )


async def list_page(world: World, args: dict, *, email: str | None = ME) -> dict:
    """One ``list_conversations`` reply (exactly one event) through the real dispatcher."""
    events: list[dict] = []
    dispatcher = FrameDispatcher(world.store, None, access=authed(email, world.org))
    await dispatcher.dispatch(json.dumps({"action": "list_conversations", "requestId": "r", **args}), events.append)
    assert len(events) == 1, events
    return events[0]


def ids(event: dict) -> list[str]:
    return [row["conversationId"] for row in event["data"]["conversations"]]


async def all_pages(world: World, limit: int, query: str | None = None) -> list[list[str]]:
    """Page through everything with ``limit``, returning each page's ids."""
    pages: list[list[str]] = []
    cursor: str | None = None
    while True:
        args: dict = {"limit": limit}
        if cursor is not None:
            args["cursor"] = cursor
        if query is not None:
            args["query"] = query
        ev = await list_page(world, args)
        assert ev["type"] == "immediate_response", ev
        pages.append(ids(ev))
        assert "nextCursor" in ev["data"], f"nextCursor is explicit, even when null: {ev}"
        has_more = ev["data"]["hasMore"]
        cursor = ev["data"]["nextCursor"]
        if cursor is None:
            assert has_more is False, f"no nextCursor means no more: {ev}"
            break
        assert has_more is True, f"a nextCursor implies hasMore: {ev}"
        assert len(pages) < 50, "paging never terminated"
    return pages


async def rest_of_pages(world: World, cursor: str, limit: int = 3) -> list[str]:
    out: list[str] = []
    next_cursor: str | None = cursor
    while next_cursor is not None:
        ev = await list_page(world, {"limit": limit, "cursor": next_cursor})
        out.extend(ids(ev))
        next_cursor = ev["data"]["nextCursor"]
    return out


# --------------------------------------------------------------------------- #
# scenario drivers — shared with the Postgres store tests
# --------------------------------------------------------------------------- #


async def check_owned_pages_are_disjoint_and_complete(w: World) -> None:
    pages = await all_pages(w, 3)
    assert [len(p) for p in pages] == [3, 3, 1]
    # Limit 3 splits the three-way timestamp tie 1 | 2: it neither repeats nor vanishes.
    assert sum(pages, []) == w.mine, "pages concatenate to the full listing, in order"


async def check_scan_pages_are_disjoint_and_complete(w: World) -> None:
    unpaged = await list_page(w, {"limit": 100})
    pages = await all_pages(w, 2)
    assert [len(p) for p in pages] == [2, 2, 2, 2]
    assert sum(pages, []) == ids(unpaged) == w.visible
    # The scope sees ownerless rows but never another member's, nor another org's.
    assert w.machine in sum(pages, [])
    assert w.theirs not in sum(pages, [])
    assert w.foreign is None or w.foreign not in sum(pages, [])


async def check_a_page_the_size_of_the_rest_reports_no_more(w: World) -> None:
    ev = await list_page(w, {"limit": 7})
    assert ids(ev) == w.mine
    assert ev["data"]["hasMore"] is False, ev
    assert "nextCursor" in ev["data"] and ev["data"]["nextCursor"] is None, ev


async def check_no_cursor_is_the_old_first_page(w: World) -> None:
    ev = await list_page(w, {})
    assert ids(ev) == w.mine, "default limit 50 returns every visible row"
    ev = await list_page(w, {"limit": 2, "cursor": ""})
    assert ids(ev) == w.mine[:2], "a blank cursor is no cursor"
    assert ev["data"]["hasMore"] is True


async def check_owned_paging_survives_concurrent_updates(w: World) -> None:
    """Bump one row on the NEXT page and one on the page already read, between pages.
    Neither may be returned twice; every untouched row must still come back; the bumped
    next-page row moves above the cursor and is not returned by the remaining pages (it
    heads a fresh first page instead)."""
    first = await list_page(w, {"limit": 3})
    page1 = ids(first)
    cursor = first["data"]["nextCursor"]
    assert cursor, first

    bumped_ahead = w.mine[4]  # would have been on page 2
    bumped_behind = page1[1]  # already returned
    for conv in (bumped_ahead, bumped_behind):
        await asyncio.sleep(0.002)
        await w.store.append_message(conv, MessageDirection.OUTBOUND, "bump")

    rest = await rest_of_pages(w, cursor)
    seen = page1 + rest
    assert len(set(seen)) == len(seen), f"no row returned twice: {seen}"
    for conv in w.mine:
        if conv != bumped_ahead:
            assert conv in seen, f"untouched row {conv} was dropped: {seen}"
    assert bumped_ahead not in rest, "a row bumped above the cursor is not on later pages"

    fresh = await list_page(w, {"limit": 1})
    assert ids(fresh) == [bumped_behind], "the latest bump heads a fresh first page"


async def check_scan_paging_survives_concurrent_updates(w: World) -> None:
    first = await list_page(w, {"limit": 3})
    cursor = first["data"]["nextCursor"]
    assert cursor, first
    await w.store.append_message(ids(first)[1], MessageDirection.OUTBOUND, "bump")
    seen = ids(first) + await rest_of_pages(w, cursor)
    assert len(set(seen)) == len(seen), f"no row returned twice: {seen}"
    for conv in w.visible:
        assert conv in seen, f"row {conv} was dropped: {seen}"


async def check_owned_search_matches_my_titles_and_never_widens_scope(w: World) -> None:
    # "acme" matches my first message, Tara's chat and my foreign-org chat; only mine
    # in this org is listed.
    ev = await list_page(w, {"query": "  AcMe "})
    assert ids(ev) == [w.acme], ev
    assert ev["data"]["conversations"][0]["title"] == "Where is the Acme invoice?"
    assert ev["data"]["hasMore"] is False
    assert ev["data"]["nextCursor"] is None


async def check_scan_search_never_returns_another_members_chat(w: World) -> None:
    ev = await list_page(w, {"query": "acme"})
    got = ids(ev)
    assert got == [w.machine, w.acme], ev  # newest first: ago(25) then ago(500)
    assert w.theirs not in got, f"another member's matching chat leaked: {ev}"
    assert w.foreign is None or w.foreign not in got, f"another org's matching chat leaked: {ev}"


async def check_search_pages_with_a_cursor(w: World) -> None:
    pages = await all_pages(w, 2, "status")
    expected = [conv for conv in w.mine if conv != w.acme]
    assert [len(p) for p in pages] == [2, 2, 2]
    assert sum(pages, []) == expected, "the six 'status update' rows, in order, once each"


async def check_an_unknown_cursor_is_a_validation_error(w: World) -> None:
    for cursor in _FOREIGN_CURSORS:
        ev = await list_page(w, {"cursor": cursor})
        assert ev["type"] == "error", (cursor, ev)
        assert ev["data"]["error"]["code"] == "VALIDATION_ERROR", (cursor, ev)


async def check_an_emailless_principal_pages_only_ownerless(w: World) -> None:
    ev = await list_page(w, {"query": "acme"}, email=None)
    assert ids(ev) == [w.machine], ev
    assert ev["data"]["hasMore"] is False


def _b64(raw: str) -> str:
    return base64.urlsafe_b64encode(raw.encode()).decode().rstrip("=")


#: Cursors no server issued: not base64url, padded, wrong alphabet, no `|`, empty id,
#: a bad or non-RFC 3339 timestamp, an all-blank string.
_FOREIGN_CURSORS = [
    "not-a-cursor",
    "bm9waXBl",  # "nopipe"
    "MjAyNi0xMC0wOFQwMDowMDowMFp8",  # "2026-10-08T00:00:00Z|" — empty id
    "not base64!",
    "   ",
    _b64("2026-10-08T00:00:00Z|abc") + "==",
    "not/base64+",
    _b64("yesterday|abc"),
    _b64("2026-10-08|abc"),
    _b64("2026-10-08T00:00:00|abc"),  # no offset
    _b64("2026-02-30T00:00:00Z|abc"),
]


# --------------------------------------------------------------------------- #
# in-memory store
# --------------------------------------------------------------------------- #


async def memory_world(*, with_machine: bool) -> World:
    store = InMemorySessionStore()
    return await build_world(store, in_memory_setter(store), with_machine=with_machine)


async def test_owned_pages_are_disjoint_and_complete() -> None:
    await check_owned_pages_are_disjoint_and_complete(await memory_world(with_machine=False))


async def test_scan_pages_are_disjoint_and_complete() -> None:
    await check_scan_pages_are_disjoint_and_complete(await memory_world(with_machine=True))


async def test_a_page_the_size_of_the_rest_reports_no_more() -> None:
    await check_a_page_the_size_of_the_rest_reports_no_more(await memory_world(with_machine=False))


async def test_no_cursor_is_the_old_first_page() -> None:
    await check_no_cursor_is_the_old_first_page(await memory_world(with_machine=False))


async def test_owned_paging_survives_concurrent_updates() -> None:
    await check_owned_paging_survives_concurrent_updates(await memory_world(with_machine=False))


async def test_scan_paging_survives_concurrent_updates() -> None:
    await check_scan_paging_survives_concurrent_updates(await memory_world(with_machine=True))


async def test_owned_search_matches_my_titles_and_never_widens_scope() -> None:
    await check_owned_search_matches_my_titles_and_never_widens_scope(await memory_world(with_machine=False))


async def test_scan_search_never_returns_another_members_chat() -> None:
    await check_scan_search_never_returns_another_members_chat(await memory_world(with_machine=True))


async def test_search_pages_with_a_cursor() -> None:
    await check_search_pages_with_a_cursor(await memory_world(with_machine=False))


async def test_an_unknown_cursor_is_a_validation_error() -> None:
    await check_an_unknown_cursor_is_a_validation_error(await memory_world(with_machine=False))


async def test_an_emailless_principal_pages_only_ownerless() -> None:
    """Rust's owned path pages nothing for an emailless principal; Python's scope for one
    is the ownerless rows (th-909995), and a search stays inside it."""
    await check_an_emailless_principal_pages_only_ownerless(await memory_world(with_machine=True))


async def test_search_matches_a_meaningful_name_but_not_the_default_placeholder() -> None:
    """The Python store has no conversation name: the title is the first inbound message,
    else the ``Conversation <id>`` placeholder. Search matches the message and never the
    placeholder (nor the id inside it)."""
    store = InMemorySessionStore()
    world = await build_world(store, in_memory_setter(store), with_machine=False)
    silent = await store.create_session("agent", None, None, owner_email=ME, enforced=True, org_id=ORG)
    await store.append_message(silent.conversation_id, MessageDirection.OUTBOUND, "agent greeting")

    listed = await list_page(world, {"limit": 1})
    assert listed["data"]["conversations"][0]["title"] == f"Conversation {silent.conversation_id}"
    for query in ("conversation", silent.conversation_id[:8], "agent greeting"):
        ev = await list_page(world, {"query": query})
        assert ids(ev) == [], (query, ev)
    ev = await list_page(world, {"query": "INVOICE"})
    assert ids(ev) == [world.acme]


async def test_unscoped_listing_pages_and_searches_every_conversation() -> None:
    """Auth disabled (the single-tenant local flavor) is unscoped: paging and search run
    over every conversation, other owners' included."""
    store = InMemorySessionStore()
    world = await build_world(store, in_memory_setter(store), with_machine=True)
    dispatcher = FrameDispatcher(store, None)  # no access → auth disabled
    events: list[dict] = []
    await dispatcher.dispatch(json.dumps({"action": "list_conversations", "query": "renewal"}), events.append)
    assert ids(events[0]) == [world.theirs]


async def test_non_string_cursor_and_query_are_ignored() -> None:
    world = await memory_world(with_machine=False)
    ev = await list_page(world, {"limit": 2, "cursor": 7, "query": None})
    assert ids(ev) == world.mine[:2]
    ev = await list_page(world, {"query": "   "})
    assert ids(ev) == world.mine, "a blank query is no filter"


# --------------------------------------------------------------------------- #
# ConversationKey / ConversationSummaryQuery — ports of adapter.rs unit tests
# --------------------------------------------------------------------------- #


def test_cursor_round_trips_at_full_precision() -> None:
    k = ConversationKey(
        datetime(2026, 10, 8, 14, 30, 0, 123456, tzinfo=timezone.utc), "3f2a0c4e-0000-4000-8000-000000000001"
    )
    assert ConversationKey.decode(k.encode()) == k
    whole = ConversationKey(datetime(2026, 10, 8, 14, 30, tzinfo=timezone.utc), "a")
    assert ConversationKey.decode(whole.encode()) == whole


def test_cursor_wire_format_is_unpadded_base64url_of_rfc3339_pipe_id() -> None:
    k = ConversationKey(
        datetime(2026, 10, 8, 14, 30, 0, 123456, tzinfo=timezone.utc), "33333333-3333-3333-3333-333333333333"
    )
    encoded = k.encode()
    assert "=" not in encoded
    assert base64.urlsafe_b64decode(encoded + "=" * (-len(encoded) % 4)).decode() == (
        "2026-10-08T14:30:00.123456Z|33333333-3333-3333-3333-333333333333"
    )
    # The cursor in spec/conformance/fixtures.json is exactly this key.
    assert encoded == "MjAyNi0xMC0wOFQxNDozMDowMC4xMjM0NTZafDMzMzMzMzMzLTMzMzMtMzMzMy0zMzMzLTMzMzMzMzMzMzMzMw"


@pytest.mark.parametrize(
    ("stamp", "want"),
    [
        # Rust emits nanoseconds when it has them; truncate to microseconds.
        ("2026-10-08T14:30:00.123456789Z", datetime(2026, 10, 8, 14, 30, 0, 123456, tzinfo=timezone.utc)),
        ("2026-10-08T14:30:00.123Z", datetime(2026, 10, 8, 14, 30, 0, 123000, tzinfo=timezone.utc)),
        ("2026-10-08T14:30:00Z", datetime(2026, 10, 8, 14, 30, tzinfo=timezone.utc)),
        ("2026-10-08t14:30:00z", datetime(2026, 10, 8, 14, 30, tzinfo=timezone.utc)),
        ("2026-10-08T16:30:00.5+02:00", datetime(2026, 10, 8, 14, 30, 0, 500000, tzinfo=timezone.utc)),
        ("2026-10-08T14:30:00.123456+00:00", datetime(2026, 10, 8, 14, 30, 0, 123456, tzinfo=timezone.utc)),
    ],
)
def test_cursor_decode_accepts_any_rfc3339(stamp: str, want: datetime) -> None:
    key = ConversationKey.decode(_b64(f"{stamp}|conv-1"))
    assert key == ConversationKey(want, "conv-1")
    assert key.updated_at.utcoffset() == timedelta(0)


def test_cursor_id_may_contain_a_pipe() -> None:
    """Split on the FIRST ``|`` (Rust ``split_once``): the timestamp never contains one."""
    key = ConversationKey.decode(_b64("2026-10-08T14:30:00Z|a|b"))
    assert key is not None and key.conversation_id == "a|b"


@pytest.mark.parametrize("bad", ["", *_FOREIGN_CURSORS])
def test_foreign_cursors_do_not_decode(bad: str) -> None:
    assert ConversationKey.decode(bad) is None


def test_precedes_is_newest_first_then_id_descending() -> None:
    def at(s: int) -> datetime:
        return datetime.fromtimestamp(s, timezone.utc)

    k = ConversationKey(at(100), "m")
    assert k.precedes(at(99), "z"), "older sorts after"
    assert not k.precedes(at(101), "a"), "newer sorts before"
    assert k.precedes(at(100), "a"), "same time, smaller id sorts after"
    assert not k.precedes(at(100), "m"), "the key itself is not after itself"
    assert not k.precedes(at(100), "z"), "same time, larger id sorts before"
    assert k.precedes(at(100) - timedelta(microseconds=1), "z"), "compared at full precision"


def test_search_matches_first_message_case_insensitively() -> None:
    q = ConversationSummaryQuery.create(10, search="  ACME ")
    assert q.search == "ACME"
    assert q.matches_search("where is the acme invoice")
    assert not q.matches_search("nothing here")
    assert not q.matches_search(None)
    blank = ConversationSummaryQuery.create(10, search="   ")
    assert blank.search is None
    assert blank.matches_search(None)
    assert blank.matches_search("anything")


async def test_a_limit_over_the_maximum_is_clamped_not_rejected() -> None:
    """A pre-paging client asking for 1000 gets the 200-row maximum and a cursor for
    the rest, not an error."""
    store = InMemorySessionStore()
    set_ts = in_memory_setter(store)
    for i in range(205):
        await seed(store, set_ts, ago(i), ME, "status update", ORG)
    world = World(store=store, org=ORG, visible=[], mine=[], acme="", theirs="", machine=None, foreign=None)
    first = await list_page(world, {"limit": 1000})
    assert first["type"] == "immediate_response", first
    assert len(ids(first)) == 200
    assert first["data"]["hasMore"] is True
    rest = await list_page(world, {"limit": 1000, "cursor": first["data"]["nextCursor"]})
    assert len(ids(rest)) == 5
    assert rest["data"]["hasMore"] is False
