---
'@smooai/smooth-operator': minor
---

Postgres storage: fix the turn-loop deadlock and wire knowledge retrieval end-to-end, plus runnable docker-compose examples.

The Postgres storage backend deadlocked on **every** turn and wedged the whole server (even `GET /health` hung). Root cause: the adapter exposes the engine's *synchronous* stores (`CheckpointStore`, `KnowledgeBase`, `Memory`, and the admin `SettingsStore` / `ConnectorConfigStore` / `IndexingStore`) over an async `deadpool` pool via a `run_blocking` bridge that spawned the future on the **server's main runtime** and then blocked the calling worker on a channel. Persona resolution calls `PgSettingsStore::get` on every turn, so every turn hit it; the spawned future never got driven and the turn never completed (confirmed by a stack sample: worker parked in `run_blocking → mpsc recv`).

Fixes (`rust/adapters/postgres`, `rust/smooth-operator-server`):

- **Dedicated bridge runtime.** The sync-over-async bridges now spawn their futures on a process-wide multi-threaded runtime whose workers are always free, so the turn's blocking wait always resolves. This is the actual deadlock fix.
- **Checkpointer.** `PostgresAdapter::checkpoints()` returns the engine's in-memory `MemoryCheckpointStore` instead of the sync r2d2 `PostgresCheckpointStore` (whose blocking `Drop`/I/O was the first-found offender). OLTP + knowledge stay Postgres-durable; only crash-resume of an in-flight turn degrades.
- **Embeddings URL.** `GatewayEmbedder` appended `/v1/embeddings` to a base URL that already ends in `/v1` (`…/v1/v1/embeddings` → 404 on every retrieval). It now appends `/embeddings`.
- **Seeding on Postgres.** `SMOOTH_AGENT_SEED_KB=1` was silently ignored for the Postgres/DynamoDB backends (`seed_knowledge` was typed to the in-memory adapter). It now seeds through the `StorageAdapter` trait, scoped to the reference org so the multi-tenant query path actually matches the seeded rows.

Verified end-to-end against `pgvector/pgvector:pg16`: streaming replies, durable conversations, human-in-the-loop approval, and knowledge retrieval with citations all work on Postgres storage; `/health` stays responsive across rapid turns.

Also adds two docker-compose example stacks — `examples/web-chat` (Vite/React PWA) and `examples/tui-chat` (terminal client) — each `docker compose up` with Postgres (pgvector) + the operator + a client, BYO OpenAI-compatible gateway via a single `.env`. The server image now defaults `SMOOTH_AGENT_BIND=0.0.0.0` (the correct container default; the process default stays loopback).
