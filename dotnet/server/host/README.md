# SmooAI.SmoothOperator.Server.Host

A **runnable** smooth-operator server in C# — wires the model, storage, auth, and GitHub
ingestion from environment config and serves the schema-driven protocol over `/ws`. This is the
deployable artifact: `dotnet run`, or build the container and ship it.

## Run

```bash
SMOOTH_GATEWAY_KEY=… \
SMOOTH_AUTH_MODE=jwt SMOOTH_JWT_HS256_SECRET=… \
SMOOTH_GITHUB_REPOS="acme/handbook,acme/runbooks@main" SMOOTH_GITHUB_TOKEN=ghp_… \
dotnet run --project dotnet/server/host
# → /health and /ws on http://localhost:5xxx
```

Or container:

```bash
docker build -f dotnet/server/host/Dockerfile -t smooth-operator-server .
docker run -p 8080:8080 -e SMOOTH_GATEWAY_KEY=… -e SMOOTH_AUTH_MODE=jwt -e SMOOTH_JWT_HS256_SECRET=… \
  -e SMOOTH_DATABASE_URL=postgres://user:pass@db:5432/smooth \
  -e SMOOTH_GITHUB_REPOS=acme/handbook -e SMOOTH_GITHUB_TOKEN=ghp_… smooth-operator-server
```

## Configuration (environment)

| Variable | Default | Notes |
| --- | --- | --- |
| `SMOOTH_GATEWAY_URL` | `https://llm.smoo.ai/v1` | Any OpenAI-compatible endpoint (smooth gateway, Azure OpenAI, Ollama). |
| `SMOOTH_GATEWAY_KEY` | — | The model API key. **Required** for chat. |
| `SMOOTH_MODEL` | `claude-haiku-4-5` | Model id at the gateway. |
| `SMOOTH_EMBEDDING_MODEL` | `text-embedding-3-small` | Embedding model for the durable knowledge store (semantic retrieval when a gateway key is set, else a deterministic fallback). |
| `SMOOTH_AGENT_RERANK` | `off` | Post-retrieval reorder stage: `gateway` (cross-encoder if keyed, else lexical), `lexical` (offline), or `off`. |
| `SMOOTH_RERANK_MODEL` | `rerank-english-v3.0` | Rerank model id when `SMOOTH_AGENT_RERANK=gateway`. |
| `SMOOTH_DATABASE_URL` | *(in-memory)* | `postgres://…` or an Npgsql connection string. Durable sessions when set. |
| `SMOOTH_AUTH_MODE` | `none` | `jwt` (verify), `trusted` (proxied identity), or `none`. |
| `SMOOTH_JWT_HS256_SECRET` | — | Shared secret when `SMOOTH_AUTH_MODE=jwt`. |
| `SMOOTH_GITHUB_REPOS` | — | Comma list of `owner/repo[@ref]` to ingest at startup. |
| `SMOOTH_GITHUB_TOKEN` | — | GitHub token for private repos / higher rate limits. |
| `SMOOTH_AGENT_PREAMBLE_MODEL` | *(unset → off)* | When set to a fast model id (e.g. `groq-gpt-oss-20b`), a small model runs IN PARALLEL with each streaming turn and emits ONE ephemeral `stream_preamble` sentence ("what I'm about to do") to cover the main model's time-to-first-token. Same gateway/key as `SMOOTH_MODEL`, capped at 64 output tokens. Best-effort: it is dropped once the real answer starts, never persisted, never in `eventual_response`, and any failure is swallowed. Unset ⇒ no extra call, behavior unchanged. |
| `SMOOTH_AGENT_CONFIRM_TOOLS` | *(unset → off)* | Comma list of tool-name substrings gated behind **write-confirmation HITL**: a turn that calls a matching tool parks and emits `write_confirmation_required`; the client resumes it with `confirm_tool_action` (`{sessionId, requestId, approved}`). Unset = no tool requires confirmation (unchanged). |

## How auth + ACL fit

Each ingested repo's docs are entitled to the group `github:owner/repo`. A connection's
`?token=` (a JWT in `jwt` mode, or proxied identity in `trusted` mode) carries the user's
`groups` (mapped from Okta/Entra). Retrieval is **scoped to those groups** — a user only ever
sees the repos they're entitled to, enforced on the live chat path. No token ⇒ anonymous ⇒
public docs only (fail-closed).

## Endpoints

- `GET /health` — liveness + the resolved model/auth mode.
- `WS /ws` — the conversation protocol (`create_conversation_session`, `send_message`, …). The
  `@smooai/chat-widget` and the polyglot clients connect here.
- `GET /admin/health` — ungated admin liveness.
- `GET /admin/me` — the resolved identity (whoami). Auth-gated (`Authorization: Bearer …`).
- `GET /admin/connectors` — the configured repos. Auth-gated.
- `POST /admin/reindex` — re-ingest every configured repo **without a restart**. Auth-gated.

Admin endpoints (except `/admin/health`) are **fail-closed**: a request that doesn't resolve to a
non-anonymous identity gets `401`. In `none` auth mode the admin API is effectively closed.
