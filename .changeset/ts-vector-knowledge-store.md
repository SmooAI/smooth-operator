---
"@smooai/smooth-operator-server": minor
---

feat(ts): durable Postgres + pgvector vector-knowledge store (polyglot parity item L)

Adds `PostgresKnowledgeStore` to the TypeScript server — the TS sibling of the Rust
`PgKnowledgeBase` (`rust/adapters/postgres/src/knowledge.rs`) and the C#
`PostgresKnowledgeBase` / `PostgresAclKnowledgeStore`. Documents are embedded (via an
injected `Embedder`; a network-free `DeterministicEmbedder` ships for tests/offline) and
stored as rows in the SAME shared `knowledge_vectors` table the Rust adapter creates, so a
row written here reads back in every other server. Retrieval ranks by pgvector cosine
distance, and document-level access control lives in the `acl` JSONB column and is filtered
IN SQL (a restricted document is never fetched) — `forAccess(access)` for the ACL-scoped
chat path, `withAcl(acl)` for ACL-stamped ingest.

Contract tests mirror the C# `KnowledgeBaseContractTests` / `AclKnowledgeContractTests`
against a real pgvector container (testcontainers, skip-if-no-Docker): ingest→retrieve,
idempotent-by-id, durability across connections, and the anonymous/entitled/unentitled ACL
leak boundary.
