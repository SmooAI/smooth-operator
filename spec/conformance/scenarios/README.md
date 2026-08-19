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

## Cancellation and Rich Interactions — the newest scenarios, and where the servers disagree

Until pearl **th-eae69d** this corpus had no `cancel` scenario and no `interaction` scenario. That was the audit's headline finding: cancellation and Rich Interactions — the two newest features — were cross-checked only by fixture *shape* (does an `interaction_required` event match its schema), never by cross-language *behavior*. A port could be fully green on parity while implementing neither correctly, and six independent reviewers found the same divergences in four different languages because nothing in CI was positioned to catch them.

Eight scenarios now cover them:

| scenario | what it pins |
|---|---|
| `cancel-mid-turn` | a cancelled turn emits terminal `cancelled` (499, echoing the **turn's** requestId) in place of `eventual_response`, and frees the turn slot |
| `cancel-no-active-turn-noop` | a `cancel` with no active turn emits **nothing** |
| `interaction-park-resume` | `identity_intake` parks behind the `identity_form` capability; a matching `submit_interaction` resumes it |
| `interaction-invalid-retryable` | invalid values → `interaction_invalid`, turn **stays parked**, twice, then a corrected submit resumes |
| `interaction-stale-id-rejected` | a stale `interactionId` → `error`/`INTERACTION_MISMATCH`, turn stays parked |
| `interaction-declined` | `declined: true` resolves the park without values |
| `interaction-conversational-fallback` | a session that did NOT declare the capability gets the text fallback, never a park |
| `interaction-choices-park-resume` | the second kind (`choices`) rides the same generic envelope |

### Making cancellation deterministic

A mock turn finishes faster than a `cancel` frame can race it, and the format has no "slow tool" directive (`server.tools` entries return a fixed string immediately). `cancel-mid-turn` therefore opens its in-flight window with a **write-confirmation park** (`server.confirmTools`) — the one pause this corpus can express. `cancel-no-active-turn-noop` asserts "nothing arrives" structurally, since no runner has a drain check: the cancel step expects zero events, so any stray event is consumed by the *next* step's first matcher and fails it.

### `knownDivergences` — an expiring marker, not a skip

A scenario may name the languages it is known to fail on today, with the reason and pearl id right next to it:

```jsonc
"knownDivergences": ["go", "typescript", "python", "dotnet"],
"knownDivergencesReason": "th-eae69d — these four emit the raise tool's toolCall chunk BEFORE interaction_required …",
```

All five runners honour it, and the contract has two halves — the second is the one that matters:

- a **listed** language that FAILS is reported (with the reason and the actual assertion) and does not fail the build;
- a **listed** language that PASSES **fails the build**, with `remove <lang> from knownDivergences in <scenario> — it now passes`.

Without that second half the markers rot silently and we recreate the exact "green tests that prove nothing" problem this corpus exists to catch. A marker is a tracked bug with an expiry, never an accepted difference — the entry comes out the moment the port is fixed, and the build tells you when that is.

Implementation note per language, since `*testing.T` and panics do not catch alike: Rust runs a marked scenario on a `tokio::spawn` handle so its panic surfaces as a `JoinError`; Go narrows the runner's `*testing.T` to a small `parityT` interface so a marked scenario can run against a recorder whose `Fatalf` panics with the message instead of failing the build; TypeScript, Python and .NET just catch the assertion. ⚠️ `go test` caches results and does not invalidate on a scenario-JSON edit — use `-count=1` when iterating locally.

### Known divergences these scenarios expose

Recorded here as facts, not as license to weaken the scenarios. **Do not "fix" a scenario to make a port pass.**

- **Park event vs. the raise tool's `stream_chunk` — Rust is 1 of 5, and Rust is right.** For a Rich Interaction, Rust emits `interaction_required` *before* the raise tool's `toolCall` chunk; Go, TypeScript, Python and .NET all emit the chunk first. **Ruled a port bug, not a protocol variant**, on three grounds: all five already defer the gated tool's chunk until after the prompt for the *other* park type (`hitl-write-confirmation`), so the four are internally inconsistent between their two park paths while Rust is consistent; Rust is the designated reference and the ports mirror it; and semantically a client that renders tool calls would otherwise show "calling `request_identity_intake`…" before the card appears, leaking framework internals ahead of the semantic event. The four ports change, not these scenarios.
- **Go's cancelled turn also trips the race detector, and a marker cannot hide that.** `go test -race` (what CI runs) reports a `DATA RACE` in core's `MockLlmProvider.ChatStream` — the cancelled turn's goroutine and the *next* turn's goroutine pop the same unguarded FIFO script concurrently. It is independent, mechanical proof of the bullet below: the cancelled turn is genuinely still executing. It is also the one failure `knownDivergences` deliberately does not tolerate — the race detector fails the test outside the runner's assertion path, so `vet-test (go/server)` stays red until Go actually aborts a cancelled turn. Do not "fix" it by guarding the mock's FIFO: that would silence the evidence while leaving the bug.

- **A cancelled turn keeps running in Go and .NET.** In both, the turn after a `cancel` produces no reply because the *cancelled* turn consumed an extra LLM response: the write-confirmation gate returns a deny instead of unwinding, the agent loop makes one more model call, and the output is merely gagged (Go: `if turnCtx.Err() != nil { return }`). Cancellation is a mute button there, not a stop button — a real cost and a real side-effect risk after a visitor hits Stop. Rust, TypeScript and Python abort the turn properly. Verified by re-running with one extra `mockLlmScript` entry: both pass, proving the entry is eaten by the cancelled turn.
- **Ack payloads differ, so only `status` is asserted on a `submit_interaction` ack.** The five servers put different fields in `data` (Go omits `kind`/`values`; Python omits `kind`, and its decline ack omits `interactionId`/`declined`; .NET's decline ack omits `declined`). Asserting more would pin one language's shape rather than the protocol's.

## Adding a scenario

Drop a `*.json` here; every server's runner picks it up automatically — there is **no per-language skip, allowlist or xfail mechanism in any of the five runners**, and no way to add one in JSON (unknown keys are silently ignored everywhere). A new scenario lands on all five simultaneously; gating one would mean editing four runners.

Two portability rules that bite:

- **Never assert a fixed number of `stream_token` events** — the mocks chunk text differently per language. Always `repeat` + `accumulate` + `assertAccumulated`, and only on the top-level `token` field.
- **Never assert `null`** to mean "field absent" — .NET's dot-path resolver returns `null` for a missing final segment while the other four fail, so such an assertion passes on exactly one server.

Still uncovered: auth gating, and graceful-drain (disconnect mid-turn → the turn still finishes).
