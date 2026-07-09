# smooth-operator · web chat example

A small, **live, runnable** chat client — a `smooth-web`-like reference UI that
drives a running `smooth-operator` server over its WebSocket protocol. It is the
interactive counterpart to `console/` (which is a read-only admin surface).

It is built entirely on the repo's own published SDK,
[`@smooai/smooth-operator`](../../typescript) — the `SmoothAgentClient` owns the
connection, request correlation, and streaming-turn lifecycle; this app adds only
the presentation model.

**What it demonstrates** (all against a real server, no mocks):

- **Connect + send + token streaming** — assistant replies grow token-by-token.
- **Inline tool-call / tool-result blocks** — text and tool chips render _in the
  order the model produced them_ (say a bit → call a tool → say a bit), and each
  chip resolves from `running…` → `✓ / ✗` as its result streams back.
- **Human-in-the-loop approvals** — a write tool that needs confirmation surfaces
  an Approve / Deny bar; the SDK resumes the exact parked turn.
- **Conversation sidebar** — `listConversations()` (newest-first) + resume + new.
- **Oldest-first history** — resumed transcripts sort ascending by `createdAt`.

The interesting code is two files: [`src/operator.ts`](src/operator.ts) (the
hook) and [`src/App.tsx`](src/App.tsx) (the UI). It's a trimmed port of the
Smooth daemon PWA's `useOperator`, re-based onto the published SDK.

## Run it

### 1. Start a smooth-operator server locally

From the repo root (see [`rust/smooth-operator-server`](../../rust/smooth-operator-server)):

```bash
# In-memory storage, seeded knowledge base, binds ws://127.0.0.1:8787/ws.
# Set SMOOAI_GATEWAY_KEY so the agent can actually run LLM turns and reply.
SMOOAI_GATEWAY_KEY=<your llm.smoo.ai key> cargo run -p smooai-smooth-operator-server
```

Without `SMOOAI_GATEWAY_KEY` the server still runs and accepts connections, but
`send_message` returns a clean error instead of a reply (you'll still see the UI
connect, and the sidebar / history work). The default gateway is
`https://llm.smoo.ai/v1`; override with `SMOOAI_GATEWAY_URL` / `SMOOTH_AGENT_MODEL`.

### 2. Start this example

```bash
pnpm install           # from the repo root (links the workspace SDK)
pnpm --filter @smooai/smooth-operator-web-chat-example dev
```

Open the printed URL (default http://localhost:5273). You should see the header
go **Ready**, then be able to chat. Ask _“What is your return policy?”_ — the
seeded server answers from its knowledge base (the seeded fact: a 17-day return
window).

### Pointing at a different server / auth

Copy `.env.example` to `.env` and set `VITE_SMOOTH_WS_URL` (and
`VITE_SMOOTH_TOKEN` for a token-gated server). Or use query params for a one-off:
`?url=ws://host/ws&token=…`. Browsers can't set WebSocket headers, so the token
rides the query string as `?token=` — exactly what the server reads it from.

## Smoke check (no browser)

A dependency-free Node driver runs the same protocol through the same SDK:

```bash
SMOOTH_WS_URL=ws://localhost:8787/ws \
  pnpm --filter @smooai/smooth-operator-web-chat-example smoke
```

It exits non-zero only if it can't connect, so it's safe to run against a keyless
dev server (it verifies connection + session + wire path, and streams a reply
when the server has a gateway key).
