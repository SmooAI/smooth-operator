# Polyglot Server Parity

How smooth-operator keeps **five native server implementations** — Rust, C#, Python, TypeScript, Go — speaking the **same wire protocol with the same behavior**, and how that's proven (not asserted) by a shared, deterministic conformance corpus.

## 1. Three surfaces, all five languages

smooth-operator is polyglot at three distinct layers. Keeping them straight is the whole game:

| Surface | What it is | Languages |
|---|---|---|
| **Engine** | the agent loop (`smooth-operator-core`) — tool-calling, knowledge grounding, HITL, compaction… | Rust · C# · Python · TS · Go (each published) |
| **Client** | a protocol client that connects to a server and speaks the wire protocol | Rust · C# · Python · TS · Go (+ a React binding + the embeddable widget) |
| **Server** | the service: WS transport, session store, per-turn engine invocation, backplane, auth | Rust (reference) · C# · Python · TS · Go |

A client never names a language, a backend, or whether the engine is embedded or remote — it only ever sees the [[Protocol Reference]]. Each **server** consumes its own language's published engine and re-exposes it over the one schema-driven WebSocket protocol (`create_conversation_session` → `send_message`+`requestId` → `stream_token`/`stream_chunk` → `eventual_response`, plus `confirm_tool_action`).

**Rust is the reference.** Where the protocol leaves a detail underspecified, Rust's behavior is canonical; the other four match it.

## 2. The parity oracle: a shared conformance corpus

Parity is not "each server passes its own tests." It's **all five produce byte-identical protocol output on a shared, deterministic corpus.**

[`spec/conformance/scenarios/*.json`](../../spec/conformance/scenarios) are language-neutral scenarios. Each is a triple:

```jsonc
{
  "mockLlmScript": [ … ],   // what the model "returns", in order — text and/or tool calls
  "server":        { … },   // optional server-side setup (tools, confirmTools, knowledge)
  "steps": [ { "send": <inbound frame>, "expect": [<outbound event matchers>] } ]
}
```

**Why it's deterministic across languages:** every server consumes the same engine, which ships a deterministic `MockLlmProvider` (record/replay). A scenario's `mockLlmScript` *is* the model's output — so the turn is deterministic, the emitted `stream_token`/`eventual_response` sequence is deterministic, and it's identical across languages. No live model, no flakiness, no creds.

Each server's test suite carries a ~one-file **runner** (the Python `python/server/tests/test_scenario_parity.py` is the reference; the others port it) that, for every scenario: boots the server in its local flavor, seeds the `server` directives, drives the `steps` over a WebSocket, and matches the `expect` event stream — normalizing non-semantic fields (ids, timestamps). When all five run the same corpus green, the servers are at **tested protocol parity**.

Matchers support `type`, `status`/`statusGte`, `assert` (dot-path, incl. array indices like `data.data.citations.0.id`), `capture` (`{{var}}` substitution), `repeat`+`accumulate`+`assertAccumulated` (the stream). See the [scenarios README](../../spec/conformance/scenarios/README.md) for the full format and the `server.{tools,confirmTools,knowledge}` directives.

## 3. What's covered (the corpus today)

| Dimension | Scenarios |
|---|---|
| **Streaming turn** | `basic-streaming-turn` (token reassembly), `multi-turn-conversation` (session continuity) |
| **Errors** (validation surface) | `unknown-session-error`, `unsupported-action-error`, `missing-action-error` — all assert the canonical `error` shape + code + echoed `requestId` |
| **Tool-call** | `tool-call-turn` — `server.tools` registers a deterministic tool; the engine calls it, the server streams the `toolCall`/`toolResult` chunks |
| **HITL** (write-confirmation) | `hitl-write-confirmation` (approve), `hitl-write-confirmation-denied` (deny), `confirm-no-pending-error` (fail-closed stray confirm) |
| **Citations** | `citations-grounded-turn` — `server.knowledge` seeds a doc; a grounded turn surfaces `data.data.citations` |

Every seam is at parity across all five servers: streaming · multi-turn · errors · tool-call · HITL (approve+deny) · graceful-SIGTERM-drain · auth-verifier (`AccessContext`/`LocalTokenVerifier`) · citations.

## 4. The corpus earns its keep

Running the shared corpus against every server has repeatedly caught **real divergences** the per-server suites missed — which is the entire point:

- **`SESSION_NOT_FOUND` / envelope error shape** — the TS and C# servers emitted `NOT_FOUND` (not the canonical `SESSION_NOT_FOUND`) and put the descriptor only under `data.error`, missing the envelope-level `error` per `spec/events/error.schema.json`. Both fixed to the reference.
- **A hidden Go skip** masking the same bug class (a `knownGoDivergence` skip map made the Go suite "pass" while the server was wrong) — removed once the server was fixed.
- **HITL ordering** — the canonical order (the gated tool's `toolCall` chunk is *deferred* until after the confirmation prompt; the confirm step is acked with `immediate_response`(200, `data.approved`) before the resumed stream) was discovered by running a draft scenario against the Rust reference — which corrected the *corpus*, not the servers.
- **Citation population** — Python hardcoded `citations=[]` and Go deferred it; the corpus surfaced the gap, both now mirror the Rust/TS/C# retrieval.

Lesson: a green per-server suite can still mask a protocol divergence (a skip, a wrong-but-self-consistent shape). The shared corpus, run against the reference, is what makes parity real.

## 5. Adding to the corpus / a new server

- **A scenario:** drop a `*.json` in `spec/conformance/scenarios/`; every server's runner globs it automatically. Pin behavior against the **Rust reference** first (run it through `rust/smooth-operator-server/tests/scenario_parity.rs`), then the others must match. Server-side setup goes in the `server` directive — each server maps it to its own mechanism (e.g. tool-call uses a tools list in Python/TS/Go/C# and the `ToolProvider` seam in Rust; the corpus is identical).
- **A new server:** port the reference runner into its suite and make the full corpus green. The five native servers were stood up this way ([[Architecture Overview]] §7).

## 6. Deployment flavors & local seams

The same Rust operator runs as different [[Deploy Architecture|deployment flavors]] selected by config — Kubernetes, AWS serverless, or a single local process (`serve_local`). The **local flavor** the smooth daemon runs adds opt-in seams (off by default; K8s/Lambda never enable them): a shared-secret `LocalTokenVerifier`, `LocalServerBuilder::auth`/`tools` (the latter via the engine's `ToolProvider` seam), and embedded widget serving. Because it *runs the operator*, it speaks the canonical protocol by construction — the official widget and all five SDK clients work natively.

## 7. CI & release notes

- Parity runs per-server in each suite (deterministic, no creds). The Rust server additionally goes through the `--locked` deploy build (kind-deploy-smoke).
- **Release lockstep:** `scripts/sync-versions.mjs` stamps the anchor version onto every published member's `Cargo.toml` **and** `rust/Cargo.lock` (keyed to the exact published-crate set, leaving `-core` and non-published crates alone) so the `--locked` build never drifts after a 🦋 release.
