---
"@smooai/smooth-operator": minor
---

fix(security): enforce tenant isolation on the by-id session paths and the knowledge store (feature gap G7)

Closes G7 with a **shared** conformance suite — `rust/adapters/multitenancy_suite.rs`,
one body run by the in-memory, Postgres and DynamoDB adapters — plus a
server-level suite driving the real `handle_frame` from an attacker in another
org. Writing it found two live cross-tenant holes.

**1. Cross-tenant session access on every by-id path (WS server + Lambda).**
The connection's org was resolved only to *stamp* newly created sessions. Every
by-id action — `get_session`, `get_conversation_messages`, `send_message`,
`confirm_tool_action`, `submit_interaction`, `verify_otp`, `rename_conversation`,
and conversation resume — went through `may_read_conversation`, which checks the
**owner email** and never the org. Its deliberate ownerless-is-open rule (a
conversation with no `user` participant carrying an email stays readable, so
anonymous principals keep their own sessions) is exactly the embeddable widget's
default state, so an attacker authenticated to org B who learned an org-A session
id could read that session, replay its whole history through a turn, retitle its
conversation, and resume it (minting a session bound to the victim's org, which
then flows into the turn's `ToolProviderContext`). The Lambda transport had **no**
check at all — `dispatch::get_session` / `send_message` acted on whatever
`storage.get_session` returned.

Fixed at the chokepoints: `scoped_session` and `may_read_conversation` now take
the connection's `auth_org` and refuse a row belonging to another tenant
(indistinguishably from not-found), and the Lambda gained the same check off the
frame's verified principal. A connection with **no** verified org (anonymous /
tokenless — the widget's normal state) is unchanged.

**2. Knowledge was not tenant-isolated on the in-memory adapter, and the admin
connector-index path ingested org-blind.** `AclKnowledgeStore` filtered by
user/group only, on the assumption that the wrapped store was already
org-partitioned — true for Postgres/DynamoDB, false for the in-memory adapter and
for any third-party adapter using the `knowledge_for_access` trait default. And
`POST /admin/connectors/{id}/index` ingested through the org-blind `knowledge()`
handle for every tenant: Postgres wrote `organization_id = NULL` (which the
org-filtered read can never match, so connector-ingested knowledge silently
returned nothing) and DynamoDB wrote whichever partition the adapter was
constructed for.

- `AclKnowledgeStore` now records each document's owning org (from the
  `org_id` metadata the ingestion pipeline stamps, falling back to the org the
  ingesting handle is bound to) and enforces the tenant boundary **before** the
  ACL.
- `DynamoKnowledgeBase` honours `AccessContext::organization_id` for the query
  partition and the document's own `org_id` for the ingest partition, mirroring
  what `PgKnowledgeBase::with_access` already did.
- `PgKnowledgeBase::ingest` prefers the document's `org_id` over the handle's, so
  the org-blind handle still lands rows in the right tenant.
- The admin index run ingests through `knowledge_for_access`.

**Behavior change worth reading before upgrading.** A retrieval whose
`AccessContext` carries an org now sees **only** documents recorded as that org's
— matching the Postgres backend's existing SQL pre-filter, so all three backends
finally agree. A document ingested through the raw `knowledge()` handle with no
`org_id` metadata belongs to no tenant and is therefore invisible to a turn that
has one. If you seed knowledge directly, either stamp `org_id` on the document or
ingest through `storage.knowledge_for_access(&AccessContext::default().with_organization_id(org))`
— which is what the reference server's seeding and the admin index path do.
