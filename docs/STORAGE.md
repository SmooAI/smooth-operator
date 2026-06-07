# Storage adapters

smooth-operator-agent never names a database in application or agent code. Everything goes through one **`StorageAdapter`** seam with two production implementations:

| | **Postgres** (k8s / self-host) | **DynamoDB** (AWS serverless) |
| --- | --- | --- |
| Conversations / participants / messages / sessions | relational tables | ElectroDB single-table |
| Connection / session WS state | table or Redis | DynamoDB (TTL) or Redis |
| Agent checkpoints | `PostgresCheckpointStore` (ships in smooth-operator) | DynamoDB checkpoint store (added here) |
| Knowledge embeddings (dense) | `pgvector` (HNSW) | **Amazon S3 Vectors** |
| Knowledge keyword (sparse) | `tsvector` BM25 | inverted-index items / managed search |

## The trait surface (planned)

```
StorageAdapter
├── conversations:  create, get, listByOrg, update
├── participants:   add, get, listByConversation, resolveByExternalId
├── messages:       append, listByConversation (paged), get
├── sessions:       create, get, update(status/counts), listByConversation
├── checkpoints:    save, loadLatest(threadId), load(id), list(threadId), prune   ← smooth-operator CheckpointStore
└── knowledge:      upsert(doc, embedding), search(queryEmbedding, k, filters)     ← smooth-operator KnowledgeBase (real impl)
```

The `checkpoints` and `knowledge` slices implement smooth-operator's `CheckpointStore` and `KnowledgeBase` traits directly, so the engine plugs straight in.

## Embedding seam (shared) and the rerank stage

**Embedding (`smooth_operator_agent_core::embedding`).** Text→vector is one shared seam, not a per-backend copy. The `Embedder` trait, the `InputType` (document vs. query) marker, the network-free `DeterministicEmbedder` (FNV-1a token hashing → L2-normalized 1024-d, reproducible with zero API calls), and the `cosine_similarity` helper all live in **core**. All three consumers — the Postgres adapter, the DynamoDB adapter, and the ingestion pipeline — import this one module (each re-exports it for source compatibility). They previously each carried a byte-identical copy, which risked silent drift (a doc embedded at ingest and a query embedded at retrieval only land close if they went through the *same* projection). A byte-identical-vector guard test pins a known input → known vector so the algorithm can't drift unnoticed. Provider-backed embedders stay with their consumer: the Postgres adapter's `GatewayEmbedder` (OpenAI-compatible `/v1/embeddings` over the SmooAI LiteLLM gateway, 1536-d) `impl`s the same core `Embedder` trait but keeps `reqwest` out of core's dense path.

**Rerank (`smooth_operator_agent_core::rerank`, Onyx-gap G8).** After hybrid retrieval (dense ∪ sparse → RRF) the top-K can be **optionally** reordered by a sharper query↔candidate relevance model before it reaches the model's context. The `Reranker` trait (`rerank(query, candidates, top_k)`) has two in-tree impls: `NoopReranker` (identity — wiring it in changes nothing, which is what makes the stage opt-in) and `LexicalReranker` (deterministic, network-free query-term-overlap / BM25-ish lexical score, offline-testable). It is wired into the `knowledge_search` tool behind `KnowledgeSearchTool::with_reranker(...)`: when set, the tool overfetches candidates and reorders them; when unset (the default) behavior is unchanged. A production cross-encoder (`GatewayReranker` — Cohere/Voyage `rerank` over the gateway) would `impl Reranker` in the **adapter** crate alongside `GatewayEmbedder`, keeping the paid API out of core; swap it in by constructing the tool with `Some(Arc::new(GatewayReranker::…))`.

## Postgres adapter (k8s)

Mirrors the smooai monorepo's schema (the north star) so dogfooding is a swap, not a rewrite:

- `conversations`, `conversation_participants` (type ∈ {user, ai-agent, human-agent}), `conversation_messages` (direction ∈ {inbound, outbound}), `conversation_sessions`.
- **Checkpoints**: `PostgresCheckpointStore` from smooth-operator (already merged — r2d2 pool, `checkpoints` table keyed `(agent_id/thread, created_at desc)`).
- **Knowledge**: a `knowledge_vectors` table with `embedding vector(1024)` (Voyage `voyage-3-large`) + `content_tsv tsvector`, HNSW cosine index. Retrieval = dense (HNSW) ∪ sparse (BM25) → Reciprocal Rank Fusion → optional rerank (the `Reranker` seam — see "Embedding seam (shared) and the rerank stage" above).

## DynamoDB adapter (AWS) — ElectroDB single-table

One table, multiple entities, modeled with [ElectroDB](https://electrodb.dev). Sketch of the access patterns and keys:

| Entity | PK | SK | Notes |
| ------ | -- | -- | ----- |
| Conversation | `ORG#<org>` | `CONV#<convId>` | list-by-org = query PK + `begins_with(SK, "CONV#")` |
| Participant | `CONV#<convId>` | `PART#<partId>` | list-by-conversation; GSI on `EXTERNAL#<externalId>` to resolve a user |
| Message | `CONV#<convId>` | `MSG#<ts>#<msgId>` | time-ordered; paged query, descending for recent |
| Session | `CONV#<convId>` | `SESS#<sessionId>` | GSI1 `SESSION#<sessionId>` → direct lookup |
| Checkpoint | `THREAD#<threadId>` | `CKPT#<zero-padded-iter>` | **latest** = query `Limit=1, ScanIndexForward=false`; history = full query; `prune` deletes oldest |
| WS connection | `CONN#<connectionId>` | `CONN#<connectionId>` | TTL attribute; GSI `SESSION#<sessionId>` for fan-out |

GSIs (overloaded): **GSI1** for session-id and external-id direct lookups; **GSI2** for connection↔session fan-out. This is textbook single-table overloading — a handful of indexes serve every access pattern.

The **checkpoint store** is the interesting one: smooth-operator's `CheckpointStore` needs `save`, `load_latest(thread)`, `load(id)`, `list(thread)`, `prune(thread, keep)`. On DynamoDB:
- `save` → `PutItem` with SK `CKPT#<zero-padded iteration>` (sortable).
- `load_latest` → `Query(PK=THREAD#…, Limit=1, ScanIndexForward=false)`.
- `list` → `Query(PK=THREAD#…)`.
- `prune` → query all, delete all but the newest `keep` (batched).
- Conversation blobs can exceed DynamoDB's 400 KB item limit for long threads → spill the serialized conversation to S3 and store the pointer (the classic large-item pattern).

## Knowledge vectors: why **not** raw DynamoDB

DynamoDB has **no vector type and no ANN/kNN index**. The only native option is to store embeddings as number lists and **brute-force scan** every item per query, computing cosine in Lambda — O(n) reads, O(n) RCUs, and latency that degrades linearly. Fine for a few hundred vectors; unusable at knowledge-base scale. A 1024-dim float32 embedding is ~4 KB, so the 400 KB item limit isn't the blocker — the missing index is.

**Decision (AWS path): Amazon S3 Vectors.** GA 2025-12-02, the first object-storage service with native vector store + similarity query. Fully serverless (no cluster to provision), scales to billions of vectors per index, ~100 ms–sub-second queries, up to ~90% cheaper than running a dedicated vector DB. It pairs cleanly with DynamoDB+ElectroDB: **DynamoDB owns the OLTP domain (conversations, checkpoints, doc metadata); S3 Vectors owns dense retrieval.** The knowledge slice writes the chunk + metadata to DynamoDB and the embedding to an S3 Vectors index keyed by the same id.

Alternatives considered:
- **OpenSearch Serverless k-NN** — powerful hybrid (vector + BM25 in one engine, closest to Onyx's Vespa), but higher floor cost and operational surface; offered as an opt-in backend for users who want managed hybrid in one place.
- **Aurora Serverless v2 + pgvector** — reuses the exact Postgres retrieval code, but Aurora Serverless v2 has a non-zero minimum ACU floor (not scale-to-zero), so it's less "serverless" than S3 Vectors for spiky workloads.

Sparse/keyword on the AWS path: start with an inverted-index-in-DynamoDB for small corpora; graduate to OpenSearch Serverless when users need real BM25 at scale. RRF fuses whichever dense + sparse arms are configured.

## Document-level access control

Org isolation (`organizationId`) is the coarse tenant boundary the knowledge slice already enforces. The **within-org, per-user/group** layer (Onyx-gap G3) sits on top in our own code via `AclKnowledgeStore`, which wraps any backend's `KnowledgeBase`, records each document's `DocAcl` at ingest, and filters query results by the requester's `AccessContext` at read (over-fetch-then-filter). It's backend-agnostic — the post-filter is identical for in-memory, Postgres, and DynamoDB. No-ACL documents default to org-public, so existing seeded knowledge stays retrievable. See [ACCESS-CONTROL.md](ACCESS-CONTROL.md).

## Conformance

Both adapters implement the same trait and pass the same conformance suite (CRUD + checkpoint round-trip + retrieval relevance fixtures), so "works on Postgres" and "works on DynamoDB" are CI-verified, not aspirational.
