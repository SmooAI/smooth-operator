# smooth-operator · terminal (TUI) chat example

A **live, runnable** terminal chat client for a running `smooth-operator` server.
It speaks the same WebSocket protocol as the [`web-chat`](../web-chat) example,
through the same published SDK ([`@smooai/smooth-operator`](../../typescript)) —
just a terminal front-end instead of a browser one.

**What it demonstrates** (all against a real server, no mocks):

- **Token streaming** — the assistant reply prints token-by-token.
- **Inline tool-call / result** — `⚙ issue_refund …` then `✓`/`✗` with the result
  (and `⚙ knowledge_search …` for the read that precedes it).
- **Human-in-the-loop approval** — the parked `issue_refund` write prompts
  `approve? [y/N]`; answering resumes the exact turn (the SDK correlates it by
  request id).
- **Durable conversations** — `/list`, `/resume <id>`, `/new` against Postgres.

The whole client is one dependency-free file: [`src/index.mjs`](src/index.mjs)
(Node 22's global `WebSocket` + `readline`; the only dependency is the SDK).

## Run it with Docker (recommended)

One command brings up Postgres (pgvector) + the operator + this client:

```bash
cp ../.env.example ../.env          # set SMOOAI_GATEWAY_KEY (see that file)
docker compose run --build --rm tui
```

First run builds the Rust server image (a few minutes; cached after). Then you're
dropped into the chat. Try:

```
you  I want to return order ORD-1234 for a refund.
```

The agent checks the seeded return policy (17-day window) and then calls the
`issue_refund` **write** tool. Because the compose file sets
`SMOOTH_AGENT_CONFIRM_TOOLS=issue_refund`, that write parks for your approval
first — say `y` to let it run.

Commands: `/new` · `/list` · `/resume <conversationId>` · `/exit`

## Run it without Docker

Against any reachable server (e.g. a local `cargo run -p smooai-smooth-operator-server`):

```bash
pnpm install    # from the repo root (links the workspace SDK)
SMOOTH_WS_URL=ws://localhost:8787/ws \
  pnpm --filter @smooai/smooth-operator-tui-example start
```

## Configuration (env)

| Var              | Default                   | Meaning                                          |
| ---------------- | ------------------------- | ------------------------------------------------ |
| `SMOOTH_WS_URL`  | `ws://localhost:8787/ws`  | Server WebSocket endpoint.                        |
| `SMOOTH_TOKEN`   | _(unset)_                 | Bearer token for a token-gated server.            |
| `SMOOTH_AGENT_ID`| random UUID               | Agent to talk to (no-auth server accepts any).    |
