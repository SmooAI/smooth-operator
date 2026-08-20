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

> ⚠️ **The runners are NOT all in the obvious project.** .NET's lives in `dotnet/server/integration-tests`, **not** `dotnet/server/tests` — running the latter gives a clean several-hundred-test pass that says nothing at all about this corpus. Verify you are running the project that actually loads these files before believing a green result.

**How to trust a green port — run the control.** A port that "passes" and a port whose runner never executed the scenario look identical from the outside, and that is the exact failure this corpus exists to catch. The cheap proof is to make it fail on purpose: add the language to a scenario's `knownDivergences`, re-run, and confirm the build fails with `remove <lang> from knownDivergences in <scenario> — it now passes`. That message can only be produced by a scenario that ran and passed. Then take the marker back out. Do this whenever a port goes green in a way that looks too easy.

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

- **~~Park event vs. the raise tool's `stream_chunk` — Rust is 1 of 5~~ — FIXED (th-ef78d0).** For a Rich Interaction, Rust emitted `interaction_required` *before* the raise tool's `toolCall` chunk while Go, TypeScript, Python and .NET emitted the chunk first. **Ruled a port bug, not a protocol variant**, on three grounds: all five already defer the gated tool's chunk until after the prompt for the *other* park type — `hitl-write-confirmation`, a scenario all five passed throughout — so the four were internally inconsistent between their own two park paths while Rust was consistent; Rust is the designated reference; and semantically a client that renders tool calls would otherwise show "calling `request_identity_intake`…" before the card appears, leaking framework internals ahead of the semantic event. **The four ports changed, and the scenarios did not.** Each port already owned the mechanism — it is the same one its write-confirmation gate uses: suppress the chunk in the engine stream loop on a tool-name predicate, then re-emit it from the park path *after* the park event. Go needed only a move (its deferral existed but re-emitted at the top of the raise tool, ahead of the park); TypeScript, Python and .NET needed the predicate added (`isInteractionRaise` / `_is_interaction_raise` / `IsInteractionRaise`, matched against the hosted kinds' `request_<kind>` names — **not** the generic `submit_interaction` tool, whose chunk would then be dropped) plus the re-emit. Every non-park exit of the raise tool (parse error, conversational fallback) re-emits the chunk immediately, so `interaction-conversational-fallback` is unchanged.
- **~~A second bug hid behind the first, in Go only~~ — FIXED (th-6fdd1c, core v1.13.2).** Fixing the order let `interaction-choices-park-resume` run past step 2 for the first time, and it then failed on Go for an unrelated reason: `splitIntoChunks` in `smooth-operator-core` Go sliced the mock reply by **bytes**, so the em-dash in "Pro it is — pulling that quote up." (36 bytes / 3 parts puts a boundary mid-rune) was split into invalid UTF-8 and each byte arrived as U+FFFD. `interaction-park-resume`'s reply is 33 bytes and its boundaries miss the rune, which is why only this one scenario tripped — with ASCII-only fixtures the bug is invisible. Both the Go chunker and the TypeScript one (UTF-16 code units: safe for the BMP, **not** for astral characters like emoji) now split on character boundaries; Python, Rust and .NET were checked and never had it. **The corpus now carries no `knownDivergences` marker at all** — every scenario passes on all five servers. That is the marker contract closing its own loop: it was re-pointed at the real remaining defect, and expired the moment that defect was fixed.
- **~~A cancelled turn keeps running in Go and .NET~~ — FIXED (th-f2ac48, PR #514).** Recorded because it is what this scenario was built to catch, and because the fix is the corpus's first end-to-end proof of itself. Both ports used to leave the turn running after a `cancel`: the write-confirmation gate returned a deny instead of unwinding, the agent loop made one more model call, and the output was merely gagged (Go: `if turnCtx.Err() != nil { return }`). Cancellation was a mute button, not a stop button — real spend and real side-effect risk after a visitor hits Stop. Two independent proofs: re-running with one extra `mockLlmScript` entry made both pass (the entry was eaten by the cancelled turn), and `go test -race` reported a `DATA RACE` in core's `MockLlmProvider.ChatStream` where the cancelled turn's goroutine and the *next* turn's goroutine popped the same unguarded FIFO concurrently. That race was the one failure `knownDivergences` deliberately did **not** tolerate — it fires outside the runner's assertion path, and suppressing a data race is the opposite of what this corpus is for. The lesson if it recurs: do not "fix" it by guarding the mock's FIFO, which silences the evidence and leaves the bug.
- **Ack payloads differ, so only `status` is asserted on a `submit_interaction` ack.** The five servers put different fields in `data` (Go omits `kind`/`values`; Python omits `kind`, and its decline ack omits `interactionId`/`declined`; .NET's decline ack omits `declined`). Asserting more would pin one language's shape rather than the protocol's.

### Park ordering — how the five actually achieve it, and why you must not copy Rust

Both park events (`write_confirmation_required` and `interaction_required`) must precede the parking tool's `toolCall` `stream_chunk`. All five servers now do this, but **not by the same mechanism**, and the difference is a trap.

In every language the chunk is emitted from the **server's own stream loop** as it consumes engine events — never from inside the engine — so it is always interceptable. What differs is Rust: it has **no ordering code at all**. Its chunk is produced by a separate event-translator task (one extra queue hop), while both park bridges write to the sink directly, so the ordering falls out of task scheduling. That is a property of `tokio`'s scheduler, and it does **not** port to goroutines, JS microtasks or .NET `Task` continuations. Reading Rust and copying what it does literally gets you nothing to copy.

The portable restatement — and what Go, TypeScript, Python and .NET all do — is the mechanism their **write-confirmation gate already used**:

1. In the stream loop, skip the `toolCall` chunk when the tool name matches a parking tool.
2. Re-emit that chunk from the park path, immediately **after** the park event.

Two footguns, both of which silently drop a chunk rather than failing loudly:

- **Match only the `request_<kind>` raise tools — never the generic `submit_interaction` tool.** It raises no park event, so nothing would ever re-emit its deferred chunk.
- **Every non-park exit of the raise tool must re-emit immediately** — the parse-error return and the conversational-fallback return. Miss these and `interaction-conversational-fallback` breaks, since that path emits no park event to trail.

## Adding a scenario

Drop a `*.json` here; every server's runner picks it up automatically — there is **no per-language skip, allowlist or xfail mechanism in any of the five runners**, and no way to add one in JSON (unknown keys are silently ignored everywhere). A new scenario lands on all five simultaneously; gating one would mean editing four runners.

Two portability rules that bite:

- **Never assert a fixed number of `stream_token` events** — the mocks chunk text differently per language. Always `repeat` + `accumulate` + `assertAccumulated`, and only on the top-level `token` field.
- **Never assert `null`** to mean "field absent" — .NET's dot-path resolver returns `null` for a missing final segment while the other four fail, so such an assertion passes on exactly one server.

Still uncovered: auth gating, and graceful-drain (disconnect mid-turn → the turn still finishes).
