<p align="center">
  <a href="https://smoo.ai"><img src="https://raw.githubusercontent.com/SmooAI/smooth-operator/main/.github/banner-python.png" alt="smooai-smooth-operator — the Python client for the smooth-operator protocol." width="100%" /></a>
</p>

<p align="center">
  <a href="https://smoo.ai"><img src="https://img.shields.io/badge/Smoo_AI-platform-00A6A6?style=for-the-badge&labelColor=020618" alt="Smoo AI"></a>
  <a href="https://github.com/SmooAI/smooth-operator/blob/main/LICENSE"><img src="https://img.shields.io/badge/license-MIT-F49F0A?style=for-the-badge&labelColor=020618" alt="license"></a>
  <a href="https://lom.smoo.ai"><img src="https://img.shields.io/badge/hosted-lom.smoo.ai-FF6B6C?style=for-the-badge&labelColor=020618" alt="lom.smoo.ai"></a>
  <a href="https://pypi.org/project/smooai-smooth-operator/"><img src="https://img.shields.io/pypi/v/smooai-smooth-operator?style=for-the-badge&labelColor=020618&color=00A6A6" alt="PyPI"></a>
  <a href="https://www.python.org"><img src="https://img.shields.io/badge/Python-%E2%89%A53.11-00A6A6?style=for-the-badge&labelColor=020618" alt="Python ≥ 3.11"></a>
</p>

<p align="center">
  <b><code>smooai-smooth-operator</code></b> — the native, fully-async Python client for the <a href="https://github.com/SmooAI/smooth-operator">smooth-operator</a> service.<br/>Streaming agent turns, HITL resume, pydantic v2 models. One of <b>five native SDKs</b> over one schema-driven WebSocket protocol.
</p>

---

## What is this?

The **native async Python client** for the [smooth-operator](https://github.com/SmooAI/smooth-operator/blob/main/docs/PROTOCOL.md) WebSocket protocol. It connects to a running smooth-operator **service** (create a session, send a message, stream the agent's events back) — not the agent engine itself. The pydantic v2 models in `smooth_operator._generated` are generated from the language-neutral JSON Schemas in [`spec/`](https://github.com/SmooAI/smooth-operator/tree/main/spec) (and committed), using pydantic discriminated unions so events deserialize to the right concrete type. The wire is camelCase; you work in idiomatic snake_case.

---

## 30-second quickstart

```bash
uv add smooai-smooth-operator   # PyPI publish pending — install from the local path today
```

Until this package is published to PyPI, install it from a sibling checkout
(`uv add ../smooth-operator/python`, or `pip install -e path/to/smooth-operator/python`).
The PyPI distribution name is **`smooai-smooth-operator`** (the import package stays
`smooth_operator`) — don't `pip install smooth-operator` from the public index until
the SmooAI release lands.

```python
import asyncio
from smooth_operator import SmoothAgentClient

async def main():
    client = SmoothAgentClient(url="ws://127.0.0.1:8787/ws")
    await client.connect()

    session = await client.create_conversation_session(agent_id=agent_id, user_name="Alice")

    turn = client.send_message(session_id=session.session_id, message="How long is your return window?")
    final = await turn                       # the terminal eventual_response
    print(final.data.payload.message_id)

asyncio.run(main())
```

(Point `url` at your own [`smooth-operator-server`](https://github.com/SmooAI/smooth-operator/blob/main/rust/README.md) or the hosted endpoint.)

---

## Watch it stream

`send_message` returns a turn you can `async for` over for live events **and** `await` for the authoritative terminal response.

```python
turn = client.send_message(session_id=session.session_id, message="Where is my order?")

async for event in turn:
    if event.type == "stream_chunk":
        print(f"\n  ↳ node: {event.node}")          # workflow node boundary
    elif event.type == "stream_token":
        print(event.token, end="", flush=True)       # tokens, live
    elif event.type == "write_confirmation_required":
        # HITL: approve, and the resumed stream flows back into this same turn.
        await client.confirm_tool_action(
            session_id=session.session_id, request_id=turn.request_id, approved=True
        )

final = await turn                                    # the terminal eventual_response
print("\nmessageId:", final.data.payload.message_id)
```

```mermaid
%%{init: {'theme':'base','themeVariables':{'background':'#020618','primaryColor':'#0b1426','primaryTextColor':'#e6edf6','primaryBorderColor':'#2b3a52','lineColor':'#7c8aa0','actorBkg':'#0b1426','actorBorder':'#2b3a52','actorTextColor':'#e6edf6','signalColor':'#7c8aa0','signalTextColor':'#e6edf6','noteBkgColor':'#f49f0a','noteTextColor':'#1a0f00','noteBorderColor':'#ff6b6c','fontFamily':'ui-sans-serif, system-ui, sans-serif'}}}%%
sequenceDiagram
  participant App
  participant C as SmoothAgentClient
  participant S as Service
  App->>C: send_message(...)
  C->>S: { action: send_message }
  S-->>C: immediate_response (202)
  S-->>C: stream_token / stream_chunk …
  S-->>C: eventual_response (200)
  C-->>App: async-for yields events · await resolves final
```

---

## camelCase wire, snake_case Python

The JSON wire form is camelCase (`requestId`, `sessionId`); the pydantic models use snake_case attributes with camelCase aliases and `populate_by_name = True`. So you construct/access with `session.session_id`, and `model_dump(by_alias=True)` emits the camelCase wire form.

---

## Polyglot — one spec, five clients

```mermaid
%%{init: {'theme':'base','themeVariables':{'background':'#020618','primaryColor':'#0b1426','primaryTextColor':'#e6edf6','primaryBorderColor':'#2b3a52','lineColor':'#7c8aa0','secondaryColor':'#0b1426','tertiaryColor':'#0b1426','fontFamily':'ui-sans-serif, system-ui, sans-serif','clusterBkg':'#0b1426','clusterBorder':'#22304a'}}}%%
flowchart LR
  SPEC["spec/ (JSON Schema)"] --> PY["Python<br/>smooth_operator"]
  SPEC --> TS["TypeScript"]
  SPEC --> GO["Go"]
  SPEC --> NET[".NET (+ MEAI IChatClient facade)"]
  SPEC --> RS["Rust"]
```

---

## Test-driven by default

> **Nothing here is vibe-coded — it's verified against a real LLM gateway.**

```mermaid
%%{init: {'theme':'base','themeVariables':{'background':'#020618','primaryColor':'#0b1426','primaryTextColor':'#e6edf6','primaryBorderColor':'#2b3a52','lineColor':'#7c8aa0','secondaryColor':'#0b1426','tertiaryColor':'#0b1426','fontFamily':'ui-sans-serif, system-ui, sans-serif','clusterBkg':'#0b1426','clusterBorder':'#22304a'}}}%%
flowchart TD
  J["🎯 LLM-as-judge quality evals (Rust harness)"]
  E["🌐 Live cross-language E2E — this client boots the real server + drives a real claude-haiku-4-5 turn"]
  C["🧪 Conformance fixtures (shared across all 5 clients)"]
  U["⚡ Unit tests (discriminated-union parsing, alias round-trip, correlation)"]
  J --> E --> C --> U
```

**26 tests.** The live cross-language E2E boots a real `smooth-operator-server` subprocess (KB seeded) and drives a real `claude-haiku-4-5` turn over WebSocket: ≥1 streamed event, a knowledge-grounded "17", per-session memory.

**A real bug the live E2E caught (mocks masked it):** `agentId` is UUID-typed in `spec/`, so pydantic rejected a bare string the lenient Go/TS clients accepted — surfacing a real cross-client `string`-vs-`UUID` alignment gap. A mock fixture using a valid UUID would have hidden it.

**The proof story:** an LLM-as-judge scored a multi-turn answer **1/5** (the runtime forgot turn 1's context); the failing eval drove a per-session-memory fix; **it now scores 5/5** — a regression a substring test would have missed. See [`docs/EVALS.md`](https://github.com/SmooAI/smooth-operator/blob/main/docs/EVALS.md).

Live tests are **gated, never silently skipped** — `SMOOTH_AGENT_E2E=1` + `SMOOAI_GATEWAY_KEY` to run; skip cleanly otherwise.

```bash
uv run pytest                                          # no creds
SMOOTH_AGENT_E2E=1 uv run pytest -m e2e                # live cross-language E2E
```

## Develop & regenerate

```bash
uv sync
uv run python -c "import smooth_operator"
uv run python scripts/generate.py    # regen pydantic models from ../spec via datamodel-code-generator
```

## Smoo-powered or bring-your-own

Point `url` at the hosted **[lom.smoo.ai](https://lom.smoo.ai)** endpoint, or at your own self-hosted `smooth-operator-server` — same protocol, same client.

## 🧩 Part of Smoo AI

`smooai-smooth-operator` is built and open-sourced by **[Smoo AI](https://smoo.ai)** — the AI-powered business platform with AI built into every product. It's the Python member of the **polyglot SDK set** (TypeScript · Python · Go · .NET · Rust) for the [smooth-operator](https://github.com/SmooAI/smooth-operator) service.

- 🌐 **The service** — [smooth-operator](https://github.com/SmooAI/smooth-operator) (protocol, server, the five clients, AWS/k8s deploy)
- 🧰 **More open source from Smoo AI** — [smoo.ai/open-source](https://smoo.ai/open-source)
- ☁️ **Hosted** — [lom.smoo.ai](https://lom.smoo.ai) runs smooth-operator for you, managed and multi-tenant

## 🔗 Links

- 📦 **PyPI** — [`smooai-smooth-operator`](https://pypi.org/project/smooai-smooth-operator/)
- 🛰️ **Protocol** — [`docs/PROTOCOL.md`](https://github.com/SmooAI/smooth-operator/blob/main/docs/PROTOCOL.md)
- 🧪 **Evals** — [`docs/EVALS.md`](https://github.com/SmooAI/smooth-operator/blob/main/docs/EVALS.md)
- 💬 **Issues** — [github.com/SmooAI/smooth-operator/issues](https://github.com/SmooAI/smooth-operator/issues)

## 📄 License

MIT © 2026 Smoo AI. See [LICENSE](https://github.com/SmooAI/smooth-operator/blob/main/LICENSE).

---

<p align="center">
  Built by <a href="https://smoo.ai"><strong>Smoo AI</strong></a> — AI built into every product.
</p>
