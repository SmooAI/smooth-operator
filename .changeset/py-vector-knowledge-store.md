---
"@smooai/smooth-operator": patch
---

feat(python): durable Postgres + pgvector knowledge store (polyglot parity item L)

Adds `PostgresVectorKnowledge` and `PostgresAclKnowledge` to the Python server —
the sibling of the Rust `PgKnowledgeBase` and the .NET `PostgresKnowledgeBase` /
`PostgresAclKnowledgeStore`. Documents are embedded (core's offline `HashEmbedder`
by default) and stored in the shared `knowledge_vectors` table; retrieval ranks by
pgvector cosine distance and returns core `KnowledgeHit`s. The ACL variant persists
a `{public, users, groups}` ACL in the `acl` jsonb column and filters by the
requester's entitlements **in SQL**, so a restricted document is never fetched —
closing the knowledge/ACL-knowledge parity gap that previously stood at
Rust + .NET only. Contract tests mirror the .NET suites, running against a real
`pgvector` container and skipping cleanly when Docker is unavailable.
