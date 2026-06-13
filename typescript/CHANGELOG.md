# @smooai/smooth-operator

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
