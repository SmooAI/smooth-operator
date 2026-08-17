"""Contract tests for the durable Postgres + pgvector knowledge stores — the Python
sibling of the .NET ``KnowledgeBaseContractTests`` / ``AclKnowledgeContractTests`` and
the Rust ``knowledge`` conformance suites.

Same behavioral contract as the in-memory retrievers, against a REAL pgvector database
in a throwaway container (testcontainers):

- ingest then retrieve ranks the relevant document first;
- ingest is idempotent by document id (one row per id);
- the ACL boundary holds — anonymous sees only public, an entitled group reads the
  private doc, an unentitled group gets no leak.

Docker is not required to run the rest of the suite: if a container cannot start these
SKIP. Uses the ``pgvector/pgvector:pg16`` image (the stock ``postgres`` image has no
``vector`` type). Each test namespaces its own organization so rows can't collide on the
one shared container.
"""

from __future__ import annotations

import asyncio
import subprocess
import uuid

import pytest

from smooth_operator_server.postgres_knowledge import AccessContext, DocumentAcl

DOCKER_PING_TIMEOUT_S = 10
CONTAINER_UP_TIMEOUT_S = 240


def _docker_reachable() -> bool:
    """Ask the Docker SERVER (not just the client) whether it's up, with a wall-clock
    bound so a dead daemon becomes a fast skip rather than a hang."""
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
def pgvector_dsn() -> str:
    """A DSN for a throwaway pgvector Postgres, or a skip."""
    if not _docker_reachable():
        pytest.skip("docker daemon not reachable")
    try:
        try:
            from testcontainers.community.postgres import PostgresContainer
        except ImportError:
            from testcontainers.postgres import PostgresContainer
    except ImportError:  # pragma: no cover - the extra is in the dev group
        pytest.skip("testcontainers[postgres] is not installed")

    container = PostgresContainer("pgvector/pgvector:pg16")
    try:
        container.start()
    except Exception as exc:  # noqa: BLE001 - any startup failure is a skip, not a failure
        pytest.skip(f"could not start pgvector container: {exc}")
    try:
        yield container.get_connection_url().replace("postgresql+psycopg2://", "postgresql://")
    finally:
        container.stop()


@pytest.fixture
async def knowledge(pgvector_dsn: str):
    """A fresh vector-knowledge store on the shared container, closed after the test."""
    from smooth_operator_server.postgres_knowledge import PostgresVectorKnowledge

    store = await asyncio.wait_for(PostgresVectorKnowledge.create(pgvector_dsn), CONTAINER_UP_TIMEOUT_S)
    try:
        yield store
    finally:
        await store.close()


@pytest.fixture
async def acl_knowledge(pgvector_dsn: str):
    from smooth_operator_server.postgres_knowledge import PostgresAclKnowledge

    store = await asyncio.wait_for(PostgresAclKnowledge.create(pgvector_dsn), CONTAINER_UP_TIMEOUT_S)
    try:
        yield store
    finally:
        await store.close()


def _org() -> str:
    """A unique organization id so a test's rows never collide with another's."""
    return f"org-{uuid.uuid4()}"


# ── Vector-knowledge contract ────────────────────────────────────────────────


async def test_ingest_then_query_ranks_relevant_doc_first(knowledge) -> None:
    org = _org()
    await knowledge.ingest(
        "Our return window is 17 days from delivery.", "returns.md", document_id="returns", organization_id=org
    )
    await knowledge.ingest(
        "Standard shipping takes 5 to 7 business days.", "shipping.md", document_id="shipping", organization_id=org
    )

    hits = await knowledge.query("how long is the return window", 4, organization_id=org)

    assert hits, "expected at least one hit"
    assert hits[0].source == "returns.md"
    assert "17 days" in hits[0].content


async def test_ingest_is_idempotent_by_id(knowledge) -> None:
    org = _org()
    await knowledge.ingest("original placeholder text", "x.md", document_id="doc-x", organization_id=org)
    await knowledge.ingest("the refreshed payload mentions wombats", "x.md", document_id="doc-x", organization_id=org)

    hits = await knowledge.query("refreshed payload wombats", 4, organization_id=org)

    matching = [h for h in hits if h.source == "x.md"]
    assert len(matching) == 1, "re-ingesting the same id must upsert, not duplicate"
    assert "wombats" in matching[0].content


async def test_org_isolation(knowledge) -> None:
    """A query scoped to one org never sees another org's documents."""
    org_a, org_b = _org(), _org()
    await knowledge.ingest("alpha org secret sauce recipe", "a.md", document_id="a", organization_id=org_a)
    await knowledge.ingest("beta org secret sauce recipe", "b.md", document_id="b", organization_id=org_b)

    hits = await knowledge.query("secret sauce recipe", 10, organization_id=org_a)
    assert {h.source for h in hits} == {"a.md"}


# ── ACL leak contract ────────────────────────────────────────────────────────


async def _seed_acl(store, org: str) -> None:
    await store.ingest(
        "Public support hours are 9 to 5.",
        "public.md",
        DocumentAcl.public_acl(),
        document_id="pub",
        organization_id=org,
    )
    await store.ingest(
        "The private launch code is hunter2.",
        "acme/private/launch.md",
        DocumentAcl.for_groups("github:acme/private"),
        document_id="secret",
        organization_id=org,
    )


async def test_anonymous_sees_only_public(acl_knowledge) -> None:
    org = _org()
    await _seed_acl(acl_knowledge, org)
    hits = await acl_knowledge.for_access(AccessContext.anon()).query("private launch code", 10, organization_id=org)
    assert all(h.source != "acme/private/launch.md" for h in hits)


async def test_entitled_user_reads_private_doc(acl_knowledge) -> None:
    org = _org()
    await _seed_acl(acl_knowledge, org)
    hits = await acl_knowledge.for_access(AccessContext.for_groups("github:acme/private")).query(
        "private launch code", 10, organization_id=org
    )
    assert any(h.source == "acme/private/launch.md" and "hunter2" in h.content for h in hits)


async def test_unentitled_user_no_leak(acl_knowledge) -> None:
    org = _org()
    await _seed_acl(acl_knowledge, org)
    hits = await acl_knowledge.for_access(AccessContext.for_groups("github:acme/other")).query(
        "private launch code hunter2", 10, organization_id=org
    )
    assert all(h.source != "acme/private/launch.md" for h in hits)
