# smooai-smooth-operator-server

<p>
  <a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>
  <a href="https://github.com/SmooAI/smooth-operator/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-FF6B6C?style=for-the-badge&labelColor=020618" alt="license"></a>
  <a href="https://www.python.org"><img src="https://img.shields.io/badge/Python-%E2%89%A53.11-00A6A6?style=for-the-badge&labelColor=020618" alt="Python ≥ 3.11"></a>
</p>

**Wiring a chat loop is a weekend project. A production agent _server_ is not.**

Sessions that survive a reconnect. A wire protocol your clients can actually speak. Streaming turns you can watch token by token. Tools the model can call — and hard limits on the ones it must never call. Human-in-the-loop when a tool wants to write.

`smooai-smooth-operator-server` is that server, async and native to Python. It speaks the [smooth-operator](https://github.com/SmooAI/smooth-operator) wire protocol ([`spec/`](https://github.com/SmooAI/smooth-operator/tree/main/spec)) and consumes the in-process [`smooai-smooth-operator-core`](https://pypi.org/project/smooai-smooth-operator-core/) engine — each turn runs a `SmoothAgent` and maps its stream onto `stream_token` / `stream_chunk` / `eventual_response`. It's the Python sibling of the [Rust](../../rust/smooth-operator-server), [Go](../../go/server), [TypeScript](../../typescript/server), and [C#](../../dotnet/server) servers, all speaking the one protocol.

> The client lives in [`python/src`](../src) (`smooai-smooth-operator`). This is the **server** half.

---

## Spin up a real agent server

```bash
python -m smooth_operator_server
# → smooth-operator-server (local flavor, python) listening on ws://127.0.0.1:8787/ws
```

That's a full agent backend — sessions, streaming turns, tool-calling, citations — on one WebSocket, in-memory, auth off, zero config. Env knobs: `SMOOTH_OPERATOR_BIND` (default `127.0.0.1:8787`), `SMOOTH_OPERATOR_SEED_KB=1` for the demo knowledge docs. The gateway is read from `SMOOAI_GATEWAY_URL` / `SMOOAI_GATEWAY_KEY` — with no key, `send_message` returns a clean `LLM_UNAVAILABLE` error and the rest of the protocol still works.

Set `SMOOTH_AGENT_PREAMBLE_MODEL` to a fast model id (e.g. `groq-gpt-oss-20b`) and every streaming turn also runs that small model **in parallel**, emitting one ephemeral `stream_preamble` sentence ("what I'm about to do") to cover the reasoning model's time-to-first-token. Same gateway/key as the main turn, capped at 64 output tokens. It is suppressed the moment the real answer starts streaming, is never folded into the reply or persisted, and any failure is swallowed. Unset (the default) ⇒ no extra call, behavior unchanged.

Or embed it in your own async app:

```python
import asyncio
from smooth_operator_server import ServerState, serve
from smooth_operator_server.session_store import InMemorySessionStore

async def main():
    state = ServerState(store=InMemorySessionStore(), chat_client=my_openai_client)
    server = await serve(state, "127.0.0.1", 0)  # port 0 → ephemeral
    print(server.ws_url())
    # ... drive real streaming turns ...
    await server.shutdown()  # graceful drain + clean exit

asyncio.run(main())
```

---

## Extensible — and safe by construction

An agent is only useful when it can *do* things, and only trustworthy when you can say what it may never do. This server gives you both seams.

**Give it your tools.** Hand engine `Tool`s to `ServerState` and they merge with the built-ins for every turn:

```python
from smooth_operator_core import Tool  # the engine's tool base

class OpenTicket(Tool):
    name = "open_ticket"
    description = "Open a support ticket for the current customer."
    parameters = {"type": "object", "properties": {"subject": {"type": "string"}}}

    async def execute(self, arguments):
        return f"ticket opened: {arguments}"

state = ServerState(
    store=InMemorySessionStore(),
    chat_client=my_client,
    tools=[OpenTicket()],
)
```

**Or let it gain tools with no redeploy.** The server hosts [SEP extensions](https://github.com/SmooAI/smooth-operator/blob/main/docs/TOOLS.md) — out-of-process tool providers discovered at runtime, their `ui/confirm` prompts bridged into the protocol's confirmation frames for HITL. Gated: an extension contributes tools **only** if you name it in `SMOOTH_EXTENSIONS_ALLOW`. Nothing loads by default.

**Now declare the lines it can't cross.** Install an `AgentConfigResolver`, and every tool — built-in, yours, or from an extension — flows through the same gates:

- **Per-agent allow-list** — an agent's `tool_config.enabledTools` restricts its turn to exactly those tools. Off the list, off the table.
- **The authLevel gate** — a tool declaring `supports_auth_requirement = True` is *blocked at call time* on a public agent when tagged `admin`, or when tagged `end_user` and the session isn't identity-verified (its OTP bit, or your `SessionAuthenticator` seam). **Fail-closed** — an absent authenticator means "not authenticated".
- **End-user OTP flow** — a refused `end_user` tool can offer a one-time-code identity flow via the `OtpService` seam; the server never generates, delivers, or validates a code (the host owns generation, delivery, expiry, attempt counting).

You decide what the agent can touch; the runner enforces it.

---

## What it does

| Piece | Module | Mirrors |
| --- | --- | --- |
| WS transport + per-connection loop + single writer | `server.py` | Rust `server.rs`, C# `SmoothOperatorWebSocketExtensions` |
| Frame dispatch (`ping` / `create` / `get` / `send_message` / `cancel`) | `dispatcher.py` | C# `FrameDispatcher`, Rust `handler.rs` |
| Session + message store | `session_store.py` | C# `SessionStore`, Rust storage adapter |
| Streaming turn (engine → protocol events) | `turn_runner.py` | C# `TurnRunner`, Rust `runner.rs` |
| Per-agent config (instructions / workflow / persona / tools) | `agent_config.py` | monorepo `agents` schema |
| Conversation-workflow steps + post-turn judge | `workflow.py` | monorepo `general-agent/workflow.ts` |
| SEP extension hosting | `extensions.py` | Rust `extensions.rs` |
| Auth verifier seam (permissive + local HS256 JWT) | `auth.py` | C# `Auth.cs`, Rust verifier seam |

**Turn cancellation (the "Stop button").** `{"action": "cancel", "requestId": "<the send_message requestId>"}` cancels the in-flight turn's `asyncio.Task` — `CancelledError` fires at its next `await`, abandoning the LLM/tool call — and emits the terminal `cancelled` event (`status: 499`, echoing the turn's `requestId`) *in place of* the `eventual_response`. One active turn per connection: a second `send_message` mid-turn is rejected with `TURN_IN_PROGRESS`, never run concurrently. A `cancel` with no active turn is a silent no-op. Partial output is discarded — the user's message is persisted at the start of the turn so it stays, the assistant reply only at the end, which the cancellation skips. A client disconnect mid-turn aborts the turn too.

**Graceful SIGTERM drain.** A shared `asyncio.Event` cancel switch is the single source of truth for "stop". Each connection loop races "cancel set" vs "next inbound frame" — with the turn dispatch awaited *inside* the frame branch, so an in-flight turn finishes before the loop exits (a drain lets a turn *complete*; only a client `cancel`/disconnect aborts one), then a backplane `detach` always runs.

**Per-agent config + conversation workflows.** `create_conversation_session` carries only an agent UUID, so config is resolved server-side per turn: an agent's `instructions` become its system prompt; `personality` / `greeting` are appended (greeting only on the first turn); a `conversation_workflow` (goal + ordered steps) renders the current step into the prompt and a cheap post-turn judge advances the pointer when the criteria are met. Parsing is tolerant (malformed → server default, never crashes a session) and the judge is failure-tolerant (any error → stay on the current step). With no resolver installed, behavior is unchanged.

---

## Five languages, one protocol

The *same* server — same wire protocol, same conformance corpus — exists in five languages. Run it where your stack already lives.

| Language | Server package | Registry |
| --- | --- | --- |
| **Python** | `smooai-smooth-operator-server` | in-repo (this package) |
| **Rust** | `smooai-smooth-operator-server` | [crates.io](https://crates.io/crates/smooai-smooth-operator-server) |
| **C# / .NET** | `SmooAI.SmoothOperator.Server` | [in-repo](../../dotnet/server) |
| **TypeScript** | `@smooai/smooth-operator-server` | [in-repo](../../typescript/server) |
| **Go** | `github.com/SmooAI/smooth-operator/go/server` | [in-repo](../../go/server) |

Every native client — [TypeScript](https://www.npmjs.com/package/@smooai/smooth-operator), Go, .NET, Python, Rust — connects to any of them unmodified.

---

## Develop

```bash
cd python/server
uv sync
uv run --quiet ruff format .
uv run --quiet ruff check .
uv run --quiet pytest -q
```

---

Part of the **[smooth-operator](https://github.com/SmooAI/smooth-operator)** service — Smoo AI's polyglot AI agent service. Don't want to run it yourself? **[lom.smoo.ai](https://lom.smoo.ai)** hosts it for you.

## License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).
