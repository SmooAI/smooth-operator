# smooth-operator · examples

Two **live, runnable** reference clients for a real `smooth-operator` server —
same protocol, same published SDK ([`@smooai/smooth-operator`](../typescript)),
two front-ends. Each is a single `docker compose` stack: **Postgres (pgvector) +
the operator server + the client**, with all the production primitives turned on.

| Example | Front-end | Run |
| --- | --- | --- |
| [`web-chat/`](web-chat/README.md) | Vite + React PWA | `docker compose up --build` → http://localhost:8080 |
| [`tui-chat/`](tui-chat/README.md) | Terminal (Node + readline) | `docker compose run --build --rm tui` |

Both demonstrate, against a real server with no mocks:

- **Token streaming** — replies grow token-by-token.
- **Knowledge retrieval** — a seeded KB (`SMOOTH_AGENT_SEED_KB=1`) grounds answers.
- **Inline tool-call / result** — the `knowledge_search` tool renders as it runs.
- **Human-in-the-loop approval** — `SMOOTH_AGENT_CONFIRM_TOOLS` parks the tool for
  a human verdict; the SDK resumes the exact turn on approve.
- **Durable conversations** — stored in Postgres; list, resume, start new.

## Provider setup — BYO gateway, one file

Copy the shared env and set your LLM gateway once. Both stacks read it.

```bash
cp .env.example .env
```

The server talks to **any OpenAI-compatible `/v1` endpoint** — Smoo AI, OpenAI,
Groq, or a local Ollama/LM Studio. Set three values in `.env`:

| Var | What |
| --- | --- |
| `SMOOAI_GATEWAY_URL` | Base URL of your gateway (default `https://llm.smoo.ai/v1`). |
| `SMOOAI_GATEWAY_KEY` | Your key. Required for real replies **and** semantic retrieval (embeddings run through the same gateway). |
| `SMOOTH_AGENT_MODEL` | A chat model your gateway exposes (default `claude-haiku-4-5`). |

Nothing is baked into an image — the key stays in your gitignored `.env`.

> **First run compiles the Rust server** (a few minutes), then it's cached. The
> operator auto-creates its schema + the `vector` extension on first connect, so
> there's nothing to migrate by hand.

`AUTH_MODE=none` in these stacks makes the server accept anonymous connections
and leaves `/admin/*` open — that's for **local demos only**. See
[`deploy/`](../deploy) for the JWT/Smoo-auth flavors you'd run in production.
