# @smooai/smooth-operator

## 0.8.0

### Minor Changes

- a8bfb62: HTTP-backed widget auth (SMOODEV-1890): `HttpWidgetAuth`, a generic `WidgetAuthProvider` that resolves each agent's embed policy (`allowed_origins` + `public_key`) by GETting `{base_url}/{agentId}` from a host policy service, with TTL caching. Response handling fails safe: 2xx caches the policy, 404 caches a no-policy result (denied under `WIDGET_AUTH_STRICT`), and 5xx/network/malformed responses return `None` without caching so the next connect retries. The server now installs it from env — set `WIDGET_AUTH_URL` (plus optional `WIDGET_AUTH_BEARER` / `WIDGET_AUTH_TTL_SECS`) to enforce embeddable-widget auth against a host's policy service with no custom binary; unset leaves the permissive default. This is the reusable mechanism a host backs with its own agent store (SmooAI points it at an api-prime route).
- bc901d7: Persistent + semantic agent memory (SMOODEV-1470, parity gap Phase 3): `PgMemory`, a pgvector-backed implementation of the core `Memory` trait in the `adapters/postgres` crate. Before this the only `Memory` backend was the core `InMemoryMemory` (a `Vec` behind a `Mutex`, keyword recall, lost on restart). `PgMemory` gives the general agent cross-thread user memory that survives restarts and recalls by semantic similarity — the Rust equivalent of the TS `store`/`store_vectors` namespaced by `['memories', orgId, userId]`.

  Each `PgMemory` instance is bound to one `(organization_id, user_id)` namespace at construction (built via `PostgresAdapter::memory(org, user)`; `user_id = None` for org-wide memory), mirroring how `PgKnowledgeBase` binds an org — the core `Memory::recall(query, limit)` signature carries no scoping, so scoping is threaded through the constructor. `store` embeds the entry content and upserts a row in a new `memories` table (`embedding vector(N)` matching the active `Embedder` dim, HNSW cosine index, namespaced by `(organization_id, user_id)`); `recall` embeds the query and returns the namespace's top-K by pgvector cosine distance with `relevance` set to the cosine similarity; `forget` deletes within the namespace. Embedding goes through the shared `Embedder` seam (DeterministicEmbedder offline, GatewayEmbedder live), so memory and knowledge vectors share column width and hashing. Covered by a testcontainers integration test (semantic recall, org/user namespace isolation, namespace-scoped forget, empty recall) that skips cleanly when Docker is unavailable. No change to the core `Memory` trait was required.

## 0.7.0

### Minor Changes

- ed12900: Realtime publish endpoint (SMOODEV-1893): `POST /admin/publish` lets non-AI publishers — job status, ingestion progress, notifications, billing — push an event to a backplane target over the WebSocket fleet without going through an agent turn. Body is `{ target: { type: session|user|org|agent|connection, id }, event }`; it calls `Backplane::publish`, so with a distributed backplane the event fans out across pods. Admin-gated (RBAC role 2); the response reports local deliveries on the serving pod (cross-pod deliveries happen but aren't counted). Targets are opaque ids matched against the connection registry — tenant id-namespacing is a host concern, documented on the handler.

## 0.6.0

### Minor Changes

- e9fa854: Distributed Backplane backends (SMOODEV-1892): `RedisBackplane` and `NatsBackplane` — the horizontal scale-out seam. Both implement the `Backplane` trait by wrapping a per-pod `InMemoryBackplane` for local registry + delivery and adding a pub/sub bus (Redis/Valkey channel or NATS subject) for cross-pod fan-out: `publish(Target, event)` delivers to local sockets immediately, then broadcasts a `BackplaneEnvelope` so every other pod re-resolves the target against its own registry and delivers to its sockets (the origin pod skips its own echo). This makes the same `publish` call reach a socket on any replica — required to run the WS service with >1 pod, and the cross-pod path for non-AI publishers. Selected at runtime via `SMOOTH_AGENT_BACKPLANE` (`memory` | `redis`/`valkey` | `nats`) + `SMOOTH_AGENT_BACKPLANE_URL`; default stays single-process in-memory. `Target` is now `Serialize`/`Deserialize` and a shared `BackplaneEnvelope` is exposed so a host's own transport adapter can speak the same wire format. New crates: `adapters/backplane-redis`, `adapters/backplane-nats` (cross-pod fan-out proven end-to-end over real Redis + NATS via testcontainers).

## 0.5.0

### Minor Changes

- e6d9dbe: Connection backplane (SMOODEV-1891): a pluggable `Backplane` trait + default `InMemoryBackplane` in the OSS server — the scale-out + event-delivery seam. Each connection's outbound sink is attached on connect and associated with its session/agent; `publish(Target, event)` delivers to every connection for a target. This is the foundation for running >1 replica (a Redis/NATS impl makes delivery cross-pod) and the plug point for non-AI realtime: any service can `publish(Target::Session(...), event)` and reach the connected client over WebSocket. Wired into `AppState` (`with_backplane`) + the connection lifecycle. Runtime-agnostic (the sink is a closure, no tokio dep added to the lib).

## 0.4.0

### Minor Changes

- 715f79c: Embeddable-widget auth (SMOODEV-1878): a pluggable `WidgetAuthProvider` hook in the Rust server that enforces a per-agent **origin allowlist** + public-key **`authContext`** (HMAC-SHA256, replay-protected) for `<smooth-agent-chat>` connections. The `Origin` header is captured at the WebSocket handshake and validated at `create_conversation_session`; hosts plug in a concrete provider (backed by their agent store) while the bundled `PermissiveWidgetAuth` leaves a standalone OSS server unaffected. `WIDGET_AUTH_STRICT=1` fails closed on unknown agents.

## 0.3.0

### Minor Changes

- 0933942: C# server (`SmooAI.SmoothOperator.Server`) + engine hardening, at Rust parity.

  Server (new):

  - Durable Postgres adapters: ACL knowledge store (ACL filtered in SQL via `acl_groups && groups`, leak contract on both in-memory and Postgres backends), session store, and checkpoint store — agent state, sessions, and ACL-scoped knowledge all survive a restart.
  - `GatewayEmbedder` for real semantic retrieval (deterministic fallback when no gateway key).
  - Reranker: opt-in post-retrieval reorder (`SMOOTH_AGENT_RERANK=gateway|lexical|off`) — engine `IReranker`/`NoopReranker`/`LexicalReranker` + server `GatewayReranker` + `RerankSelection`, wired through the turn; fails soft if the reranker errors.
  - Auth-gated `/admin` API: `/admin/health`, `/admin/me`, `/admin/connectors`, and `POST /admin/reindex` (re-ingest without a restart); fail-closed Bearer auth.
  - Tool `stream_chunk`s: tool call/result surfaced over the WebSocket protocol.
  - Deployable host (`SmooAI.SmoothOperator.Server.Host`) + Dockerfile: wires gateway model, storage, JWT/trusted/none auth, and startup GitHub ingestion.

  Engine (`SmooAI.SmoothOperator.Core`):

  - `IReranker` + `NoopReranker` + `LexicalReranker` + `Rerankers.ApplyOptionalAsync`.
  - `RunStreamingAsync` now yields the tool-result update so tool results surface in the stream.

  Robustness fixes:

  - Chunker no longer infinite-loops on long non-whitespace runs (minified code / base64 / long URLs).
  - The dispatcher emits a clean error and keeps the connection alive on any handler exception (was dropping the socket silently).
  - Postgres checkpoint store preserves tool-call/result content (was serializing text only).
  - GitHub connector fails loud on a truncated tree instead of silently indexing a partial repo.
