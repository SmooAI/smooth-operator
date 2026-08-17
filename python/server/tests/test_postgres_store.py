"""Round-trip tests for the durable Postgres store, against a REAL Postgres in a
throwaway container (testcontainers) — the Python sibling of the Rust adapter's
``conformance.rs`` / ``admin_conformance.rs``, the Go ``postgres_store_test.go``, the
TS ``postgres-store.test.ts``, and the C# ``PostgresSessionStoreTests``.

Docker is not required to run the rest of the suite: if a container cannot start,
every Postgres test SKIPS. The "memory stays the default" tests need no container at
all — they are the guard that the in-memory path is untouched when
``SMOOTH_AGENT_STORAGE`` is unset.

Local gotcha: on OrbStack, testcontainers' Ryuk reaper can hang before the database
container is ever started, and these all skip on the timeout with Docker plainly
running. ``TESTCONTAINERS_RYUK_DISABLED=true uv run pytest`` gets past it (at the cost
of leaving containers behind to clean up by hand). CI's plain dockerd runs Ryuk fine,
so this stays off by default.
"""

from __future__ import annotations

import asyncio
import subprocess
import uuid
from datetime import datetime, timedelta, timezone

import pytest

from smooth_operator_server.admin import InMemoryAdminStore
from smooth_operator_server.session_store import InMemorySessionStore, MessageDirection

#: How long to wait for the Docker daemon to answer, and for a container to be serving.
#: The ping is short because its only job is to turn "no daemon" into a fast skip; the
#: start is generous because a cold machine pulls the image first.
DOCKER_PING_TIMEOUT_S = 10
CONTAINER_UP_TIMEOUT_S = 240


def _docker_reachable() -> bool:
    """Ask the SERVER (not just the client binary) whether Docker is up.

    The subprocess timeout is the point: against a dead daemon the docker CLI can block
    indefinitely, and testcontainers shells out to it. Without a wall-clock bound the
    intended skip becomes a hang that pytest eventually reports as a FAILURE — a guard
    that fails open like that is worse than no guard.
    """
    try:
        subprocess.run(
            ["docker", "version", "--format", "{{.Server.Version}}"],
            capture_output=True,
            check=True,
            timeout=DOCKER_PING_TIMEOUT_S,
        )
        return True
    except (OSError, subprocess.SubprocessError):
        return False


@pytest.fixture(scope="session")
def postgres_dsn() -> str:
    """A DSN for a throwaway Postgres, or a skip. One container for the whole session;
    every test namespaces its own org so they cannot see each other's rows."""
    if not _docker_reachable():
        pytest.skip("docker daemon not reachable")
    try:
        # The module moved to `testcontainers.community.postgres`; the old path still
        # works but warns. Try the new one first so the suite is quiet on both.
        try:
            from testcontainers.community.postgres import PostgresContainer
        except ImportError:
            from testcontainers.postgres import PostgresContainer
    except ImportError:  # pragma: no cover - the extra is in the dev group
        pytest.skip("testcontainers[postgres] is not installed")

    container = PostgresContainer("postgres:16-alpine")
    try:
        container.start()
    except Exception as exc:  # noqa: BLE001 - any startup failure is a skip, not a failure
        pytest.skip(f"could not start postgres container: {exc}")
    try:
        # asyncpg speaks plain `postgresql://`, not SQLAlchemy's `postgresql+psycopg2://`.
        yield container.get_connection_url().replace("postgresql+psycopg2://", "postgresql://")
    finally:
        container.stop()


@pytest.fixture
async def store(postgres_dsn: str):
    """A fresh store on the shared container, closed when the test ends."""
    from smooth_operator_server.postgres_store import PostgresStore

    created = await asyncio.wait_for(PostgresStore.create(postgres_dsn), CONTAINER_UP_TIMEOUT_S)
    try:
        yield created
    finally:
        await created.close()


def org(prefix: str = "org") -> str:
    return f"{prefix}-{uuid.uuid4()}"


# ── sessions / conversations / messages ─────────────────────────────────────


async def test_survives_a_new_connection(store, postgres_dsn: str) -> None:
    """The durability claim itself: a session and its messages written through one
    store are readable through a SECOND store on the same database — i.e. they survive
    the process that wrote them, which is the whole point of this backend."""
    from smooth_operator_server.postgres_store import PostgresStore

    org_id = org()
    created = await store.create_session(
        "", "Alice", "alice@example.test", owner_email="alice@example.test", org_id=org_id
    )
    uuid.UUID(created.session_id)  # raises if it isn't a uuid
    await store.append_message(created.conversation_id, MessageDirection.INBOUND, "hello")
    await store.append_message(created.conversation_id, MessageDirection.OUTBOUND, "hi there")

    # A brand-new store handle — nothing carried over in process memory.
    reopened = await PostgresStore.create(postgres_dsn)
    try:
        fetched = await reopened.get_session(created.session_id)
        assert fetched is not None
        assert fetched.conversation_id == created.conversation_id
        assert fetched.agent_id == created.agent_id
        assert fetched.agent_participant_id == created.agent_participant_id
        assert fetched.owner_email == "alice@example.test"
        assert fetched.contact_email == "alice@example.test"

        messages = await reopened.list_messages(created.conversation_id, 50)
        assert [(m.direction, m.text) for m in messages] == [
            (MessageDirection.INBOUND, "hello"),
            (MessageDirection.OUTBOUND, "hi there"),
        ]
        assert messages[0].created_at is not None

        assert await reopened.get_session("does-not-exist") is None
    finally:
        await reopened.close()


async def test_list_messages_respects_limit(store) -> None:
    """The most recent ``limit``, oldest first — the in-memory contract."""
    session = await store.create_session("", "Alice", None, org_id=org())
    for i in range(5):
        await store.append_message(session.conversation_id, MessageDirection.INBOUND, f"m{i}")

    assert [m.text for m in await store.list_messages(session.conversation_id, 2)] == ["m3", "m4"]
    assert len(await store.list_messages(session.conversation_id, 0)) == 5


async def test_resume_is_owner_scoped_without_an_oracle(store) -> None:
    """Resume binds the caller's OWN conversation; someone else's takes the identical
    branch as an unknown id, so it cannot be used to probe which ids exist."""
    org_id = org()
    owned = await store.create_session(
        "", "Alice", "alice@example.test", owner_email="alice@example.test", enforced=True, org_id=org_id
    )

    resumed = await store.create_session(
        "",
        "Alice",
        "alice@example.test",
        owned.conversation_id,
        owner_email="alice@example.test",
        enforced=True,
        org_id=org_id,
    )
    assert resumed.conversation_id == owned.conversation_id
    assert resumed.owner_email == "alice@example.test"

    # Bob names Alice's conversation …
    bob = await store.create_session(
        "",
        "Bob",
        "bob@example.test",
        owned.conversation_id,
        owner_email="bob@example.test",
        enforced=True,
        org_id=org_id,
    )
    assert bob.conversation_id != owned.conversation_id
    # … and gets exactly what he gets for an id that never existed.
    unknown = await store.create_session(
        "", "Bob", "bob@example.test", str(uuid.uuid4()), owner_email="bob@example.test", enforced=True, org_id=org_id
    )
    assert unknown.conversation_id != owned.conversation_id

    # The resume must not have re-homed the conversation onto Bob.
    after = await store.get_session(owned.session_id)
    assert after is not None and after.owner_email == "alice@example.test"


async def test_ownerless_conversation_stays_reachable(store) -> None:
    """An ownerless conversation (auth disabled, or an emailless principal) stays
    reachable — denying it locks anonymous visitors out of what they just created."""
    org_id = org()
    anonymous = await store.create_session("", "", None, enforced=True, org_id=org_id)
    assert anonymous.owner_email is None

    carol = await store.create_session(
        "",
        "Carol",
        "carol@example.test",
        anonymous.conversation_id,
        owner_email="carol@example.test",
        enforced=True,
        org_id=org_id,
    )
    assert carol.conversation_id == anonymous.conversation_id


async def test_list_conversations_is_scoped(store) -> None:
    """Owner-scoped, with empty conversations dropped."""
    org_id = org()
    alice = await store.create_session(
        "", "Alice", "alice@example.test", owner_email="alice@example.test", enforced=True, org_id=org_id
    )
    await store.append_message(alice.conversation_id, MessageDirection.INBOUND, "alice asks")
    await store.append_message(alice.conversation_id, MessageDirection.OUTBOUND, "agent answers")

    bob = await store.create_session(
        "", "Bob", "bob@example.test", owner_email="bob@example.test", enforced=True, org_id=org_id
    )
    await store.append_message(bob.conversation_id, MessageDirection.INBOUND, "bob asks")

    # An empty conversation (every page-load mints one) must not show up.
    await store.create_session(
        "", "Alice", "alice@example.test", owner_email="alice@example.test", enforced=True, org_id=org_id
    )

    seen = await store.list_conversations("alice@example.test", enforced=True, org_id=org_id)
    assert len(seen) == 1
    assert seen[0].conversation_id == alice.conversation_id
    assert seen[0].message_count == 2
    assert seen[0].first_inbound_text == "alice asks"
    assert seen[0].updated_at is not None

    bob_sees = await store.list_conversations("bob@example.test", enforced=True, org_id=org_id)
    assert [c.conversation_id for c in bob_sees] == [bob.conversation_id]


async def test_unscoped_sees_every_conversation_in_its_org(store) -> None:
    """The auth-disabled flavor (``enforced=False``) is unscoped by OWNER. It is the
    path a laptop actually runs on, and it still must not cross orgs."""
    org_id = org()
    for email in ("alice@example.test", None):
        session = await store.create_session("", "U", email, owner_email=email, org_id=org_id)
        await store.append_message(session.conversation_id, MessageDirection.INBOUND, "hi")

    assert len(await store.list_conversations(None, enforced=False, org_id=org_id)) == 2
    assert await store.list_conversations(None, enforced=False, org_id=org("other")) == []


async def test_isolates_organizations(store) -> None:
    """Org is the OUTER scope. Driven with the SAME email in two orgs, so only the org
    can be doing the isolating."""
    org_a, org_b = org("a"), org("b")
    in_a = await store.create_session(
        "", "Shared", "shared@example.test", owner_email="shared@example.test", enforced=True, org_id=org_a
    )
    await store.append_message(in_a.conversation_id, MessageDirection.INBOUND, "org A only")

    assert await store.list_conversations("shared@example.test", enforced=True, org_id=org_b) == []

    cross_org = await store.create_session(
        "",
        "Shared",
        "shared@example.test",
        in_a.conversation_id,
        owner_email="shared@example.test",
        enforced=True,
        org_id=org_b,
    )
    assert cross_org.conversation_id != in_a.conversation_id


async def test_persists_workflow_step_and_otp_bit(store, postgres_dsn: str) -> None:
    """Both survive a reconnect, and both are no-ops for an unknown id."""
    from smooth_operator_server.postgres_store import PostgresStore

    session = await store.create_session(
        "", "Alice", "alice@example.test", owner_email="alice@example.test", org_id=org()
    )
    await store.set_current_step_id(session.conversation_id, "collect-email")
    await store.set_session_authenticated(session.session_id, True)

    reopened = await PostgresStore.create(postgres_dsn)
    try:
        assert await reopened.get_current_step_id(session.conversation_id) == "collect-email"
        assert await reopened.is_session_authenticated(session.session_id) is True
        # The OTP write must not have clobbered the contact email beside it.
        fetched = await reopened.get_session(session.session_id)
        assert fetched is not None and fetched.contact_email == "alice@example.test"

        # Clearing the step removes only that key.
        await reopened.set_current_step_id(session.conversation_id, None)
        assert await reopened.get_current_step_id(session.conversation_id) is None

        await reopened.set_session_authenticated(session.session_id, False)
        assert await reopened.is_session_authenticated(session.session_id) is False

        # No-ops for unknown ids, never errors.
        await reopened.set_current_step_id("unknown-conversation", "whatever")
        await reopened.set_session_authenticated("unknown-session", True)
        assert await reopened.is_session_authenticated("unknown-session") is False
    finally:
        await reopened.close()


async def test_reports_owner_org_so_the_gate_can_enforce_it(store) -> None:
    """The durable store must report the conversation's ORG on the session, not just the
    owner. The dispatcher treats an unrecorded org as "fall through to ownership", so a
    store that leaves it None reopens the cross-org hole for ownerless conversations
    while every existing test still passes. Uses an OWNERLESS conversation — an owned
    one would pass for the wrong reason, since ownership alone blocks the cross-org read."""
    org_id = org()
    anonymous = await store.create_session("", "", None, org_id=org_id)
    assert anonymous.owner_email is None
    assert anonymous.owner_org == org_id

    fetched = await store.get_session(anonymous.session_id)
    assert fetched is not None
    assert fetched.owner_org == org_id, "the gate cannot enforce an org it is never told"


# ── admin stores ────────────────────────────────────────────────────────────


async def test_connectors_are_org_scoped(store, postgres_dsn: str) -> None:
    from smooth_operator_server.postgres_store import PostgresStore

    org_a, org_b = org("a"), org("b")
    now = datetime.now(timezone.utc).isoformat()
    zendesk = {
        "id": str(uuid.uuid4()),
        "name": "zendesk",
        "kind": "helpdesk",
        "config": {"subdomain": "acme"},
        "enabled": True,
        "createdAt": now,
        "updatedAt": now,
        "_orgId": org_a,
    }
    await store.put_connector(zendesk)
    await store.put_connector(
        {**zendesk, "id": str(uuid.uuid4()), "name": "algolia", "kind": "search", "config": {}, "enabled": False}
    )

    # Read back through a fresh connection — durability, not a process-local dict.
    reopened = await PostgresStore.create(postgres_dsn)
    try:
        rows = await reopened.list_connectors(org_a)
        assert [c["name"] for c in rows] == ["algolia", "zendesk"]
        assert rows[1]["config"] == {"subdomain": "acme"}
        assert rows[1]["enabled"] is True

        # Org B sees nothing; a cross-org id reports exactly like an unknown one.
        assert await reopened.list_connectors(org_b) == []
        assert await reopened.get_connector(org_b, zendesk["id"]) is None
        assert await reopened.get_connector(org_b, str(uuid.uuid4())) is None
        assert await reopened.delete_connector(org_b, zendesk["id"]) is False

        # Upsert updates in place rather than duplicating.
        await reopened.put_connector({**zendesk, "name": "zendesk-eu", "enabled": False})
        updated = await reopened.get_connector(org_a, zendesk["id"])
        assert updated is not None and updated["name"] == "zendesk-eu" and updated["enabled"] is False
        assert len(await reopened.list_connectors(org_a)) == 2

        assert await reopened.delete_connector(org_a, zendesk["id"]) is True
        assert await reopened.get_connector(org_a, zendesk["id"]) is None
    finally:
        await reopened.close()


async def test_settings_round_trip(store, postgres_dsn: str) -> None:
    from smooth_operator_server.postgres_store import PostgresStore

    org_id = org()
    # An unset org reports None so the handler can substitute defaults.
    assert await store.get_settings(org_id) is None

    written = {
        "orgId": org_id,
        "model": "claude-haiku-4-5",
        "systemPrompt": "be brief",
        "defaultTools": ["search", "email"],
        "updatedAt": datetime.now(timezone.utc).isoformat(),
    }
    await store.put_settings(written)

    reopened = await PostgresStore.create(postgres_dsn)
    try:
        read = await reopened.get_settings(org_id)
        assert read is not None
        assert read["model"] == written["model"]
        assert read["systemPrompt"] == written["systemPrompt"]
        assert read["defaultTools"] == ["search", "email"]

        # One row per org: a second put replaces rather than duplicating.
        await reopened.put_settings({**written, "model": "claude-sonnet-5"})
        updated = await reopened.get_settings(org_id)
        assert updated is not None and updated["model"] == "claude-sonnet-5"

        assert await reopened.get_settings(org()) is None
    finally:
        await reopened.close()


async def test_indexing_runs_are_org_scoped(store, postgres_dsn: str) -> None:
    from smooth_operator_server.postgres_store import PostgresStore

    org_a, org_b = org("a"), org("b")
    started = datetime.now(timezone.utc)
    finished = started + timedelta(seconds=1)
    run = {
        "id": str(uuid.uuid4()),
        "connectorName": "zendesk",
        "status": "succeeded",
        "startedAt": started.isoformat(),
        "finishedAt": finished.isoformat(),
        "documentsSeen": 7,
        "chunksIndexed": 21,
        "documentsSkipped": 1,
        "error": None,
        "_orgId": org_a,
    }
    await store.record_run(run)
    await store.record_run(
        {**run, "id": str(uuid.uuid4()), "connectorName": "algolia", "status": "failed", "_orgId": org_b}
    )

    reopened = await PostgresStore.create(postgres_dsn)
    try:
        runs = await reopened.list_runs(org_a)
        assert len(runs) == 1
        assert runs[0]["id"] == run["id"]
        assert runs[0]["connectorName"] == "zendesk"
        assert runs[0]["status"] == "succeeded"
        assert (runs[0]["documentsSeen"], runs[0]["chunksIndexed"], runs[0]["documentsSkipped"]) == (7, 21, 1)
        assert runs[0]["error"] is None
        assert datetime.fromisoformat(runs[0]["finishedAt"]) == finished

        # Re-recording the same id updates in place.
        await reopened.record_run({**run, "status": "failed", "error": "boom"})
        after = await reopened.list_runs(org_a)
        assert len(after) == 1
        assert after[0]["status"] == "failed"
        assert after[0]["error"] == "boom"
    finally:
        await reopened.close()


# ── memory stays the default ────────────────────────────────────────────────


async def test_resolve_storage_returns_none_for_memory() -> None:
    """The guard on the whole swap: with SMOOTH_AGENT_STORAGE unset (or memory) nothing
    durable is resolved. Needs no Docker."""
    from smooth_operator_server.postgres_store import resolve_storage

    assert await resolve_storage({}) is None
    assert await resolve_storage({"SMOOTH_AGENT_STORAGE": "memory"}) is None
    # An ambient DATABASE_URL alone must never switch the backend.
    assert await resolve_storage({"DATABASE_URL": "postgresql://nope/nope"}) is None


async def test_resolve_storage_rejects_misconfiguration() -> None:
    """A durable backend that cannot be configured is fatal, never a silent fall back
    to memory — losing durability quietly is the failure worth shouting about."""
    from smooth_operator_server.postgres_store import resolve_storage

    with pytest.raises(ValueError, match="neither SMOOTH_AGENT_DATABASE_URL nor DATABASE_URL"):
        await resolve_storage({"SMOOTH_AGENT_STORAGE": "postgres"})
    with pytest.raises(ValueError, match="unknown SMOOTH_AGENT_STORAGE"):
        await resolve_storage({"SMOOTH_AGENT_STORAGE": "cassandra"})


async def test_in_memory_stores_are_unchanged() -> None:
    """The in-memory stores keep behaving exactly as they did: the added ``org_id``
    argument is accepted and ignored, so a single-tenant caller sees no change."""
    memory = InMemorySessionStore()
    session = await memory.create_session("agent-1", "Alice", "alice@example.test", owner_email="alice@example.test")
    await memory.append_message(session.conversation_id, MessageDirection.INBOUND, "hello")

    # Passing an org changes nothing for the memory store.
    assert len(await memory.list_conversations("alice@example.test", enforced=True, org_id="some-org")) == 1
    assert len(await memory.list_conversations("alice@example.test", enforced=True)) == 1

    admin = InMemoryAdminStore()
    assert await admin.list_connectors("public") == []
    assert await admin.get_settings("public") is None
    assert await admin.delete_connector("public", "nope") is False
