# smooai-smooth-operator-server

A native, async **WebSocket server** for the smooth-operator protocol — the Python
parity of the Rust (`rust/smooth-operator-server`) and C# (`dotnet/server`)
reference servers. It consumes the in-process [`smooai-smooth-operator-core`](https://pypi.org/project/smooai-smooth-operator-core/)
engine: each turn runs a `SmoothAgent` and maps its stream events onto the wire
protocol's `stream_token` / `stream_chunk` / `eventual_response` events.

This is the **server** side. The client lives in `python/src` (`smooai-smooth-operator`).

## Run it (local flavor — in-memory, auth off, zero config)

```bash
python -m smooth_operator_server
# → smooth-operator-server (local flavor, python) listening on ws://127.0.0.1:8787/ws
```

Env knobs: `SMOOTH_OPERATOR_BIND` (default `127.0.0.1:8787`), `SMOOTH_OPERATOR_SEED_KB=1`
to load the demo knowledge docs. The LLM gateway is read from `SMOOAI_GATEWAY_URL` /
`SMOOAI_GATEWAY_KEY` — with no key, `send_message` returns a clean `LLM_UNAVAILABLE`
error and the rest of the protocol (create/get session, ping) still works.

## Embed it

```python
import asyncio
from smooth_operator_server import ServerState, serve
from smooth_operator_server.session_store import InMemorySessionStore

async def main():
    state = ServerState(store=InMemorySessionStore(), chat_client=my_openai_client)
    server = await serve(state, "127.0.0.1", 0)  # port 0 → ephemeral
    print(server.ws_url())
    # ... use it ...
    await server.shutdown()  # graceful drain + await clean exit

asyncio.run(main())
```

## What it does

| Piece | Module | Mirrors |
| --- | --- | --- |
| WS transport + per-connection loop + single writer | `server.py` | Rust `server.rs`, C# `SmoothOperatorWebSocketExtensions` |
| Frame dispatch (`ping` / `create` / `get` / `send_message`) | `dispatcher.py` | C# `FrameDispatcher`, Rust `handler.rs` |
| Session + message store | `session_store.py` | C# `SessionStore`, Rust storage adapter |
| Streaming turn (engine → protocol events) | `turn_runner.py` | C# `TurnRunner`, Rust `runner.rs` |
| Per-agent config parse (instructions / workflow / persona) | `agent_config.py` | monorepo `agents` schema |
| Conversation-workflow step helpers + post-turn judge | `workflow.py` | monorepo `general-agent/workflow.ts` + `workflow-judge.ts` |
| Protocol event builders | `protocol.py` | C# `ProtocolEvents`, Rust `protocol.rs` |
| Auth verifier seam (permissive + local HS256 JWT) | `auth.py` | C# `Auth.cs`, Rust verifier seam |

### Graceful SIGTERM drain

A shared `asyncio.Event` cancel switch on `ServerState` is the single source of
truth for "stop". `SIGTERM`/`SIGINT` (when `install_signal_handlers=True`) stop
accepting new connections and set the cancel. Each connection loop checks the cancel
first every iteration, then races "cancel set" vs "next inbound frame" — with the
turn dispatch awaited **inside** the frame branch, so an in-flight turn finishes
before the loop exits. A backplane `detach` always runs after the loop (the
detach-after-loop). The Redis/NATS cross-pod backplane the Rust server supports is
left as a seam (`backplane.py`).

### Per-agent config + conversation workflows (SMOODEV-590)

`ServerState.agent_config_resolver` (an `AgentConfigResolver`) looks up an
`AgentConfig` by `agentId`, server-side — the ws protocol's
`create_conversation_session` carries only an agent UUID, so config can't ride
the payload. The default `StaticAgentConfigResolver({...})` is dict-backed (empty
= no-op); a multi-tenant host swaps in one backed by the `agents` table. Resolved
per turn from the session's agent, it lets each agent override the server-wide
`system_prompt`:

- **`instructions`** — the agent's system-prompt body (falls back to
  `system_prompt`, then the built-in default).
- **`personality` / `greeting`** — appended to the prompt (greeting only on the
  first turn of a conversation).
- **`conversation_workflow`** — a `goal` + ordered `steps` (`id` / `intent` /
  `criteria` / optional `next`). The current step is rendered into the prompt;
  after each turn a cheap judge call decides whether the criteria were met and
  advances the pointer (explicit `next` → sequential → terminal). The step id is
  tracked per conversation.
- **`tool_config`** (or `allowedTools`) — a string-array tool allow-list. When
  non-empty, the agent's turns are restricted to the server tools whose names
  appear in it (empty/absent → the full server tool set; unknown names ignored).

Config is parsed tolerantly (malformed → server default, never crashes a
session) and the judge is failure-tolerant (any error → stay on the current
step). With `agent_configs` empty, behavior is unchanged.

## Develop

```bash
cd python/server
uv sync
uv run --quiet ruff format .
uv run --quiet ruff check .
uv run --quiet pytest -q
```
