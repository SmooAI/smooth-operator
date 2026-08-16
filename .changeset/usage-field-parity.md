---
'@smooai/smooth-operator': minor
---

feat(go,python,ts,dotnet): emit `eventual_response.usage` — a spec field that was implemented 1-of-5

`eventual-response.schema.json` has defined an optional `usage` object
(`{ costUsd, promptTokens, completionTokens }`) since cost reporting landed, but
only the Rust server ever put it on the wire. The other four engines all track
per-turn token accounting already — the data existed and was dropped on the floor
at the last hop, so a client on any non-Rust server could not accumulate session
cost at all.

Each server now captures the turn's accumulated usage from its engine's terminal
completion event and threads it onto `eventual_response`, matching the Rust
reference's semantics exactly: the key is **omitted entirely** when the engine
reported no usage, so the event stays byte-identical for clients that predate the
field.

- **Go** — captured at `core.StreamDone` off `AgentRunResponse`, carried on `TurnResult.Usage`.
- **Python** — captured at `DoneEvent` off `response.usage` / `response.cost_usd`.
- **TypeScript** — captured at the `done` stream event off `response.usage` / `response.costUsd`.
- **C#** — the engine's `RunStreamingAsync` surfaces no terminal usage total, so the
  server accumulates the model's own `UsageContent` chunks over the turn, which is
  the same total once the stream ends. Token counts are real; `costUsd` is 0 until a
  pricing table is wired (see below).

Two limitations worth knowing, both pre-existing and documented in
`spec/conformance/scenarios/README.md` rather than papered over:

- **`costUsd` is 0 on every non-Rust server.** None of them wires a pricing table
  onto its engine, so the cost tracker prices every call at 0. Only Rust reports a
  real figure, which it reads from the gateway's cost header. The token counts are
  unaffected.
- **The scenario corpus cannot assert `usage` yet.** The five mock LLM providers
  disagree on what a scripted turn reports (Go/Python/TS 0/0, Rust 0/5, C# 10/5),
  and the Rust and C# scenario runners strict-compare JSON numbers in opposite
  directions (`0.0` vs `0`). Aligning the mocks in `smooth-operator-core` and
  comparing numbers by value would make `usage` a real parity assertion. Until then
  each server's protocol unit tests cover the contract that matters — omitted when
  absent, all three fields when present.
