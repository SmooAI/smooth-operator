# Server scenario conformance — the polyglot parity corpus

`spec/conformance/fixtures.json` pins the **shape** of individual protocol messages. These **scenarios** go one level up: they pin the **behavior of a full server** — a sequence of inbound frames and the exact outbound event stream every server must produce in response.

This is how the five native servers (Rust · C# · Python · TypeScript · Go) are held **to parity**. Each language's server test suite runs the *same* JSON scenarios through its own server and asserts the *same* normalized output. Parity is no longer "each server passes its own tests" — it's "all five produce identical protocol output on a shared corpus."

## Why it's deterministic across languages

Every server consumes the same engine (`smooth-operator-core`), which ships a deterministic **`MockLlmProvider`** (record/replay). A scenario's `mockLlmScript` *is* the model's output — so the turn is deterministic, the emitted `stream_token` / `eventual_response` sequence is deterministic, and it's identical across languages. No live model, no flakiness.

## Scenario format

```jsonc
{
  "name": "basic-streaming-turn",
  "description": "...",
  "mockLlmScript": [ { "kind": "text", "text": "Hello from the engine!" } ],  // what the model returns, in order
  "steps": [
    {
      "send": { "action": "...", "requestId": "...", ... },   // one inbound frame ({{var}} templating allowed)
      "expect": [                                             // the outbound events this frame must produce, in order
        { "type": "immediate_response", "status": 200, "capture": { "sessionId": "data.sessionId" } },
        { "type": "stream_token", "repeat": true, "accumulate": "token",
          "assertAccumulated": "Hello from the engine!" },
        { "type": "eventual_response", "status": 200,
          "assert": { "data.data.response.responseParts": ["Hello from the engine!"] } }
      ]
    }
  ]
}
```

**`mockLlmScript`** — ordered model outputs. `{ "kind": "text", "text": "..." }` (one assistant turn of text); `{ "kind": "toolCall", "name": "...", "arguments": "{...}" }` (a tool call). The runner loads these into the engine's `MockLlmProvider` before driving the server.

**`server`** *(optional)* — server-side setup the runner applies before booting, so a scenario can exercise more than a bare chat turn:

- **`server.tools`** — deterministic tools to register on the agent. Each is `{ name, description, parameters, result }`; the tool ignores its arguments and returns the fixed `result` string, so a tool-calling turn is fully reproducible. A `mockLlmScript` `toolCall` entry names one of these; the server dispatches it and streams a `stream_chunk` with `data.state.rawResponse.toolCall` then one with `data.state.rawResponse.toolResult` before the final text. Each server maps this onto its own tool-injection mechanism (a tools list for Python/TS/Go/C#; the `ToolProvider` seam for Rust) — the corpus is identical.
- **`server.confirmTools`** — tool-name patterns gated by **write-confirmation HITL**. When the engine calls a matching tool, the server **parks** the turn and emits `write_confirmation_required` (with `data.data.{ toolId, actionDescription }`) instead of running it; the scenario then sends a `confirm_tool_action` frame (`sessionId` + `approved`), the server acks with `immediate_response`(200, `data.approved`), and the parked turn resumes (runs the tool on approve, rejects on deny). The gated tool's `toolCall` chunk is deferred until *after* the confirmation prompt. Canonical order verified against the Rust reference.
- **`server.knowledge`** — docs `{ source, content }` seeded into the server's knowledge base before the turn, so a grounded answer surfaces **citations**. The server mirrors the engine's auto-retrieval (`query(message, 3)`) into `eventual_response`'s `data.data.citations[]` — each `{ id, title, url?, snippet, score }`, present only when non-empty. Assert the deterministic fields (`citations.N.id`/`title`/`snippet`) via array-index paths; **not** `score` (a computed float). Each server seeds its own KB the same way (the runner sets the doc id to its source so `id == title == source` is deterministic). Canonical fields verified against the Rust reference.

### `eventual_response.usage` — asserted (tokens), deliberately not asserted (`costUsd`)

All five servers attach the optional `usage` object (`{ costUsd, promptTokens, completionTokens }`) to `eventual_response`, and `basic-streaming-turn` now asserts its **token counts** as a real cross-language invariant: a scripted turn reports **10 prompt / 5 completion** on every server. Two upstream fixes made that possible (pearl th-4f1263):

- **The five mock providers agree** (`smooth-operator-core` ≥ 1.7.15). They used to disagree — Go/Python/TypeScript reported `0/0`, C# `10/5`, and Rust `0/5` that wasn't even a mock decision: the mock reported `0/0` and the engine's streaming path estimated completion tokens from the reply's *length*, so the number moved whenever a scenario's text changed. All five now report `10/5/15` behind a named helper (`scripted_usage()` / `ScriptedUsage()` / `SCRIPTED_USAGE`), attached only by the FIFO scripting helpers so a *drained* script still reports nothing.
- **All five runners compare JSON numbers BY VALUE.** Rust and C# used to strict-compare in *opposite* directions (Rust emitted `0.0` and rejected an integer `0`; C# emitted `0` and rejected `0.0`) while Go/Python/TS already compared loosely.

> ⚠️ Rust's runner must push a `StreamEvent::Usage(scripted_usage())` into the stream it synthesizes. The sibling mocks synthesize their own final usage chunk from the FIFO script; Rust's `MockLlmClient` takes an explicit event list, and without that event the engine falls back to its length-based estimator. Drop it and this scenario goes red with a number derived from the reply text.

**`costUsd` is deliberately NOT asserted**, and this is not an oversight to fix later. It is a function of which model a server names and which pricing table its engine carries, and those legitimately differ: Go/Python/TypeScript engines ship a `DEFAULT_PRICING` containing `claude-haiku-4-5` — which *is* the engine default their servers fall back to — so a mock turn prices at roughly $0.000035, while Rust's substring resolver returns free for that name and C# had no default table at all until `smooth-operator-core` ≥ 1.7.17. Pinning one number here would be over-fitting a production-pricing concern into a protocol-parity test. Per-language unit tests cover the shape (the key is omitted when the engine reported nothing, and carries all three fields when it did).

**`steps[].send`** — one inbound protocol frame. `{{name}}` placeholders are substituted from values `capture`d earlier (e.g. `"sessionId": "{{sessionId}}"`).

**`steps[].expect`** — the outbound events the frame must produce, **in order**. Each matcher:
| field | meaning |
|---|---|
| `type` | required outbound event `type` (`immediate_response`, `stream_token`, `eventual_response`, `error`, …). |
| `status` / `statusGte` | assert `status` equals / is ≥ the value. |
| `capture` | `{ var: "dot.path" }` — grab a field into a variable for later `{{var}}` substitution. |
| `assert` | `{ "dot.path": value }` — assert fields equal the given values. |
| `repeat` | `true` → this matcher consumes one-or-more consecutive events of `type` (e.g. the stream). |
| `accumulate` | with `repeat`, concatenate this string field across the repeated events. |
| `assertAccumulated` | assert the concatenation equals the value (e.g. the streamed text reassembles to the engine's reply). |

## Normalization

The runner compares only the fields a matcher names. Non-deterministic, non-semantic fields — `messageId`, server-generated ids, `timestamp` — are **not** asserted unless a scenario explicitly does so. Ordering of the named events is significant; interleaved keepalive/ping frames are ignored.

## The per-server runner contract

Each server provides a small test that, for every `*.json` here:
1. starts the server in its **local flavor** with the engine's `MockLlmProvider` seeded from `mockLlmScript`;
2. opens a protocol WebSocket client;
3. for each step: substitutes `{{vars}}`, sends `send`, then consumes + matches `expect` (capturing vars, accumulating, asserting);
4. shuts down.

The **Python reference runner** is [`python/server/tests/test_scenario_parity.py`](../../../python/server/tests/test_scenario_parity.py) — port its ~80 lines into the TS/Go/C#/Rust server suites. When all five run this corpus green, the servers are at protocol parity.

## Adding a scenario

Drop a `*.json` here; every server's runner picks it up automatically. Cover: multi-turn, tool-call + `confirm_tool_action` (HITL), citations, auth gating, error frames, and graceful-drain (cancel mid-turn → the turn still finishes).
